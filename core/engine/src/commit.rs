// SPDX-License-Identifier: Apache-2.0

//! Building commits from local changes and acknowledging their results (docs/design/sync-engine.md
//! sections 2.4, 2.6, 2.7, 2.8 and 4.2, and RFC 0002): the rebase of concurrent changes onto R, the
//! one commit in flight with its history and device slot, and what each result and error does.

use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, VecDeque};

use koidrive_proto::v1::{self as proto, error, op, op_result};
use prost::Message;

use crate::classify::{Class, Origin, Trees, Waiting, path};
use crate::effects;
use crate::first_link::{self, History, Marks, PriorSync};
use crate::guard::{self, Guard, RemoteRemoval};
use crate::ids::{IdGen, unix_ms};
use crate::names::{MAX_NAME_BYTES, name_key, nfc};
use crate::question::{
    Answer, ConflictFile, Question, QuestionKind, RecoveryCause, RecoveryGroup, RecoveryTrigger,
    Revisions,
};
use crate::recovery;
use crate::restore::{Outcome, RESOLVED_KEPT, Rejection, Resolved, RestoreId, RestoreIntent};
use crate::state::{Agreed, CommitsChanges, CommitsState, FinishOrigin, StateError};
use crate::tracked::{Marked, TrackedMap, TrackedSet};
use crate::tree::{Content, Kind, NodeId, Shape, same, same_content};

/// The most ops one commit holds (spec v0 section 13).
pub const MAX_OPS: usize = 1000;
/// How many of this device's acknowledged commits [`Commits::held_through`] remembers. A server
/// restored from a backup older than all of them gives 0, so no lost delete is sent again.
pub const SLOTS_KEPT: usize = 1000;

/// The most bytes of change records a commit is planned to make, as estimated. Its records and
/// its result must each fit in one 1 MiB `ServerMessage` (spec v0 section 7.2), and the estimate is
/// rough, so it stays well below.
pub const MAX_COMMIT_BYTES: usize = 512 * 1024;

/// What one change record costs besides the node's name and content: its IDs, revisions, version
/// and framing.
const RECORD_BYTES: usize = 256;

/// The seq a commit builds on and the commit token there (RFC 0013): the `commit_id` and
/// `committed_at_ms` of the commit R applied at that seq. An empty token claims nothing, so a
/// server checks only the seq.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Base {
    pub seq: u64,
    pub commit_id: String,
    pub committed_at_ms: i64,
}

impl From<u64> for Base {
    fn from(seq: u64) -> Self {
        Self {
            seq,
            ..Self::default()
        }
    }
}

/// The commit in flight as it is persisted before it is sent (section 2.2).
#[derive(Clone, Debug, PartialEq)]
pub struct InFlight {
    pub commit: proto::Commit,
    /// For each `Delete` op, by its target, every node the client planned to delete with it.
    pub deletes: BTreeMap<NodeId, Vec<NodeId>>,
    /// For each `Restore` op, by its index in the commit, the nodes this device's user removed that
    /// it brings back. When the restore is applied, they are downloaded instead of deleted again.
    pub restores: BTreeMap<usize, Vec<NodeId>>,
    /// The result once it has arrived, kept until R holds its records.
    pub result: Option<proto::CommitResult>,
}

impl InFlight {
    pub(crate) fn positions(&self, id: &str) -> bool {
        (self.commit.ops.iter()).any(|op| op.op.as_ref().and_then(positioned) == Some(id))
    }
}

/// Why no commit is built or resent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Halt {
    /// The server takes no writes from this device. Downloads continue, and a `Welcome` that
    /// allows writes lifts it.
    ReadOnly,
    /// The store cannot account for the server's state (sections 2.8 and 6.4). The commit in flight
    /// is kept as evidence and nothing is planned from this store.
    Recovery(RecoveryCause),
    /// A policy halt (RFC 0006 section 1): the server refused the commit with `code`, which only
    /// the user or a client update can resolve. The commit in flight is kept with its slot and
    /// never resent, nothing more is planned, and downloads continue. No `Welcome` lifts it, and
    /// it is part of the stored state (section 2.8), so it outlasts a restart.
    Policy { code: i32 },
}

/// What to do with the commit in flight after connecting (section 2.8, RFC 0002 section 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reconnect {
    /// Nothing to send now.
    Idle,
    /// Send the commit in flight again, unchanged.
    Resend,
    /// Enter recovery.
    Recovery,
    /// The server lost commits of this device that it had accepted: list the tree again, as for
    /// `AHEAD_OF_SERVER` (section 4.4), so that what it lost is sent again.
    AheadOfServer,
}

/// How a person lifts a policy halt (RFC 0006 section 1, point 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lift {
    /// Send the refused commit again, unchanged, once the person fixed the cause on the server, such
    /// as the access a `FORBIDDEN` refusal named.
    Resend,
    /// Forget the refused commit, so its changes are planned again into a commit with a new
    /// `commit_id`, as after `UNSUPPORTED_VERSION`. A refused commit stores nothing, so its slot
    /// stays free.
    Replan,
}

/// Why [`Commits::lift`] lifted nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiftError {
    /// No policy halt holds.
    NotHalted,
    /// The halt is a `CONFIRMATION_REQUIRED`, which only the owner's confirmation lifts (RFC 0006
    /// section 10), and no RFC defines that confirmation yet.
    NeedsConfirmation,
}

/// What a call-level error on the commit in flight leads to (section 4.2, RFC 0002 section 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Send the same commit again: after uploading its chunks again, after the back-off the error
    /// names, or after signing in again.
    Resend,
    /// Nothing was applied and the commit is forgotten. Its slot stays free, and S never changed
    /// for it, so its changes are planned again.
    Abandoned,
    /// Abandoned, and the server is behind what this device saw: bootstrap before planning again.
    Resync,
    /// The commit did not fit (`LIMIT_EXCEEDED`). Nothing was applied and the commit is
    /// forgotten; its changes are planned again in commits half its size, a delete that was
    /// alone in its commit is planned as deletes of its contents first, a restore that was alone
    /// ends as [`Outcome::DoesNotFit`], and any other op alone in its commit marks its node as a
    /// problem.
    Split,
    Halted(Halt),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitError {
    NotInFlight,
    /// The result does not have one outcome per op.
    ResultMismatch,
}

/// The commit side of a space: the accepted device counter, the commit in flight, the nodes the
/// server rejected an op for, the restores a person asked for, and whether committing has stopped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commits {
    accepted: Marked<u64>,
    in_flight: Marked<Option<InFlight>>,
    problems: TrackedSet<NodeId>,
    halted: Marked<Option<Halt>>,
    /// The most ops the next commit holds after a `LIMIT_EXCEEDED`, doubled back towards
    /// [`MAX_OPS`] by every acknowledged commit.
    cap: Marked<Option<usize>>,
    /// Folders whose delete did not fit alone, deleted contents first.
    split: TrackedSet<NodeId>,
    /// Folders merged into one L holds elsewhere, by the folder they were merged into. Creates and
    /// moves into them go to that folder, and the nodes they bring are reverted, so on disk they
    /// follow into it. An alias ends with its folder's revert: a folder the user moves elsewhere
    /// is then created on its own, and what is created in it goes into it.
    aliases: TrackedMap<NodeId, NodeId>,
    /// Restores without an outcome, in request order (see [`Commits::restore`]).
    restores: Marked<BTreeMap<RestoreId, RestoreIntent>>,
    /// The last [`RESOLVED_KEPT`] restores that ended, oldest first.
    resolved: Marked<Vec<Resolved>>,
    /// The seq of each of this device's last [`SLOTS_KEPT`] acknowledged commits, and of the commit
    /// in flight once R holds its record, by slot.
    slots: TrackedMap<u64, u64>,
    /// The open questions, in the order they were asked.
    questions: Marked<Vec<Question>>,
    /// The local entries a [`Answer::Publish`] to a first link's question released (section 6.4),
    /// until the server applies their create. The person chose to keep them beside what R holds,
    /// so a create of one that takes a conflict name asks nothing.
    published: TrackedSet<NodeId>,
    guard: Guard,
    granted: Unstored<Vec<NodeId>>,
}

#[derive(Clone, Debug, Default)]
struct Unstored<T>(T);

impl<T> PartialEq for Unstored<T> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Commits {
    /// What the store keeps of the commit side.
    pub fn state(&self) -> CommitsState {
        CommitsState {
            accepted: *self.accepted,
            in_flight: (*self.in_flight).clone(),
            problems: (*self.problems).clone(),
            halted: *self.halted,
            cap: *self.cap,
            split: (*self.split).clone(),
            aliases: (*self.aliases).clone(),
            restores: (*self.restores).clone(),
            resolved: (*self.resolved).clone(),
            slots: (*self.slots).clone(),
            questions: (*self.questions).clone(),
            published: (*self.published).clone(),
            guard: self.guard.state(),
        }
    }

    /// What changed in [`Commits::state`] since the last call, for the store to write.
    pub fn take_changes(&mut self) -> CommitsChanges {
        let Self {
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
            guard,
            granted: _,
        } = self;
        CommitsChanges {
            accepted: accepted.take_changed().copied(),
            in_flight: in_flight.take_changed().cloned(),
            problems: problems.take_changes(),
            halted: halted.take_changed().copied(),
            cap: cap.take_changed().copied(),
            split: split.take_changes(),
            aliases: aliases.take_changes(),
            restores: restores.take_changed().cloned(),
            resolved: resolved.take_changed().cloned(),
            slots: slots.take_changes(),
            questions: questions.take_changed().cloned(),
            published: published.take_changes(),
            guard: guard.take_changes(),
        }
    }

    /// Continues from `accepted`, the server's count of this device's commits, in a store that
    /// never synced the space. Nothing else changes.
    pub fn adopt(&mut self, accepted: u64) {
        self.accepted.set(accepted);
    }

    /// The commit side the store kept. Refuses a commit in flight that does not take slot
    /// `accepted + 1`, whose result does not have one outcome per op, or that lists planned
    /// deletes for a node none of its `Delete` ops targets, or that holds a `Restore` op no queued
    /// restore asks for. Also refuses a cap of 0 or of at least [`MAX_OPS`], a folder merged into
    /// itself, and a restore that has an outcome and is still queued or has a second outcome.
    pub fn from_state(state: CommitsState) -> Result<Self, StateError> {
        if let Some(flight) = &state.in_flight {
            let device_seq = flight.commit.device_seq;
            if state.accepted.checked_add(1) != Some(device_seq) {
                return Err(StateError::DeviceSeq {
                    accepted: state.accepted,
                    device_seq,
                });
            }
            let ops = &flight.commit.ops;
            if let Some(result) = &flight.result
                && result.results.len() != ops.len()
            {
                return Err(StateError::ResultMismatch);
            }
            let deleted = |id: &str| {
                ops.iter().any(
                    |op| matches!(&op.op, Some(op::Op::Delete(delete)) if delete.node_id == id),
                )
            };
            if let Some(id) = flight.deletes.keys().find(|id| !deleted(id)) {
                return Err(StateError::DeleteWithoutOp(id.clone()));
            }
            carried(&state.restores, ops).map_err(StateError::RestoreNotQueued)?;
        }
        let mut ended = BTreeSet::new();
        for resolved in &state.resolved {
            if state.restores.contains_key(&resolved.id) || !ended.insert(&resolved.id) {
                return Err(StateError::RestoreResolved(resolved.id.clone()));
            }
        }
        if let Some(cap) = state.cap.filter(|cap| !(1..MAX_OPS).contains(cap)) {
            return Err(StateError::Cap(cap));
        }
        if let Some((id, _)) = state.aliases.iter().find(|(id, into)| id == into) {
            return Err(StateError::SelfAlias(id.clone()));
        }
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
            guard,
        } = state;
        Ok(Self {
            accepted: accepted.into(),
            in_flight: in_flight.into(),
            problems: problems.into(),
            halted: halted.into(),
            cap: cap.into(),
            split: split.into(),
            aliases: aliases.into(),
            restores: restores.into(),
            resolved: resolved.into(),
            slots: slots.into(),
            questions: questions.into(),
            published: published.into(),
            guard: guard.into(),
            granted: Unstored::default(),
        })
    }

    /// The seq of the newest commit of this device that the server counts, or 0 when none is
    /// known. A server that lost commits keeps every seq up to the backup it was restored from, and
    /// this commit is among them, so a node state of a seq at or below it is from before the loss
    /// (section 4.4).
    pub fn held_through(&self) -> u64 {
        (self.slots.range(..=*self.accepted).next_back()).map_or(0, |(_, &seq)| seq)
    }

    /// Whether one of this device's commits has seq `seq`: one of the last [`SLOTS_KEPT`]
    /// acknowledged, or one whose record R held while it was in flight ([`Commits::own_record`]). A
    /// commit the server lost is included until R is listed again ([`Commits::forget_lost_slots`]).
    pub fn made_here(&self, seq: u64) -> bool {
        self.slots.values().any(|&made| made == seq)
    }

    /// Whether the record of the commit of `seq` and `commit_id` is this device's: one of an
    /// acknowledged commit, or one of the commit in flight, which can arrive before its result. The
    /// record of the commit in flight gives it slot `seq`, so a loss that drops the commit before its
    /// result still finds the commit made here.
    pub fn own_record(&mut self, seq: u64, commit_id: &str) -> bool {
        let Some(slot) = (self.in_flight.as_ref())
            .filter(|flight| flight.commit.commit_id == commit_id)
            .map(|flight| flight.commit.device_seq)
        else {
            return self.made_here(seq);
        };
        self.slots.insert(slot, seq);
        true
    }

    /// Whether the commit in flight is `commit_id` and its last op that sets the content of `id` is
    /// an edit other than a resend: a record of it that arrives before the result is this device's
    /// edit.
    pub fn edits_in_flight(&self, commit_id: &str, id: &str) -> bool {
        (self.in_flight.as_ref()).is_some_and(|flight| {
            flight.commit.commit_id == commit_id
                && (flight.commit.ops.iter().rev()).find_map(|op| match &op.op {
                    Some(op::Op::UpdateContent(update)) if update.node_id == id => {
                        Some(!update.resend)
                    }
                    Some(op::Op::Restore(restore))
                        if restore.node_id == id && !restore.version_id.is_empty() =>
                    {
                        Some(false)
                    }
                    _ => None,
                }) == Some(true)
        })
    }

    /// Whether the commit in flight is `commit_id` and its edit of `id` answers a conflict
    /// question: the file takes the content of its copy, which L does not hold for it.
    pub fn answers_in_flight(&self, commit_id: &str, id: &str) -> bool {
        (self.in_flight.as_ref()).is_some_and(|flight| {
            flight.commit.commit_id == commit_id
                && (flight.commit.ops.iter().filter_map(|op| op.op.as_ref())).any(|op| {
                    matches!(op, op::Op::UpdateContent(update) if update.node_id == id)
                        && self.answer_for(commit_id, op).is_some()
                })
        })
    }

    /// The files the commit in flight edits, other than by a resend.
    pub fn edited_in_flight(&self) -> Vec<NodeId> {
        let ops = (self.in_flight.as_ref()).map_or(&[][..], |flight| &flight.commit.ops[..]);
        (ops.iter())
            .filter_map(|op| match &op.op {
                Some(op::Op::UpdateContent(update)) if !update.resend => {
                    Some(update.node_id.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Whether a listed node at revision `seq` was placed there by this device: by a commit of
    /// this device that the server kept, or by the commit in flight, whose result has that seq.
    pub fn listed_here(&self, seq: u64) -> bool {
        (self.made_here(seq) && !self.lost_here(seq))
            || (self.in_flight.as_ref())
                .and_then(|flight| flight.result.as_ref())
                .is_some_and(|result| result.seq == seq)
    }

    /// Whether one of this device's commits that the server lost has seq `seq`: one above
    /// [`Commits::held_through`], until R is listed again.
    pub fn lost_here(&self, seq: u64) -> bool {
        self.made_here(seq) && seq > self.held_through()
    }

    /// Forgets the slots above the accepted counter, those of the commits a loss took, once R is
    /// listed again and [`Trees::forget_lost`] no longer needs them.
    pub fn forget_lost_slots(&mut self) {
        let accepted = *self.accepted;
        self.slots.retain(|slot, _| *slot <= accepted);
    }

    /// The accepted counter *a* of section 2.8: the `device_seq` of the last commit whose result
    /// was applied.
    pub fn accepted(&self) -> u64 {
        *self.accepted
    }

    pub fn in_flight(&self) -> Option<&InFlight> {
        self.in_flight.as_ref()
    }

    /// Nodes an op was rejected for. They are left out of planning until [`Commits::clear_problem`],
    /// after the next rescan or a user action.
    pub fn problems(&self) -> &BTreeSet<NodeId> {
        &self.problems
    }

    pub fn clear_problem(&mut self, node: &str) -> bool {
        self.problems.remove(node)
    }

    pub fn halted(&self) -> Option<Halt> {
        *self.halted
    }

    /// Lifts a policy halt the way a person chose. With [`Lift::Resend`] the commit in flight
    /// stays, and the next `Welcome` that accounts for its slot sends it again (section 2.8); a
    /// server loss under the halt has already dropped it, and its changes are planned again. With
    /// [`Lift::Replan`] it is forgotten, and its restores stay queued for the next commit, as when
    /// a refusal forgets a commit. Nothing else changes.
    pub fn lift(&mut self, lift: Lift) -> Result<(), LiftError> {
        let Some(Halt::Policy { code }) = *self.halted else {
            return Err(LiftError::NotHalted);
        };
        if code == error::Code::ConfirmationRequired as i32 {
            return Err(LiftError::NeedsConfirmation);
        }
        self.halted.set(None);
        if lift == Lift::Replan {
            self.in_flight.set(None);
        }
        Ok(())
    }

    /// Queues a restore a person asked for. The next commit that is built carries its `Restore`
    /// op, and acknowledging that commit moves the restore to [`Commits::resolved`] with its
    /// outcome. On a read-only device it ends at once as rejected. False, and nothing changes,
    /// when a restore with this ID is queued or has an outcome, so a request can be repeated.
    pub fn restore(&mut self, id: RestoreId, intent: RestoreIntent) -> bool {
        if self.restores.contains_key(&id) || self.resolved.iter().any(|ended| ended.id == id) {
            return false;
        }
        self.restores.get_mut().insert(id, intent);
        self.reject_unsent_when_read_only();
        true
    }

    /// The restores without an outcome, sent or not, in request order.
    pub fn restores(&self) -> &BTreeMap<RestoreId, RestoreIntent> {
        &self.restores
    }

    /// The queued restores that wait because their node is a problem
    /// ([`RestoreIntent::held_by`]), so a surface can say why they are pending.
    pub fn held_restores(&self) -> impl Iterator<Item = &RestoreId> {
        let held = |(_, intent): &(&RestoreId, &RestoreIntent)| intent.held_by(&self.problems);
        self.restores.iter().filter(held).map(|(id, _)| id)
    }

    /// The last [`RESOLVED_KEPT`] restores that ended, oldest first. An older outcome is dropped.
    pub fn resolved(&self) -> &[Resolved] {
        &self.resolved
    }

    /// The open questions, in the order they were asked. No commit carries an op for a node one of
    /// them holds ([`Question::holds`]) until it is answered.
    pub fn questions(&self) -> &[Question] {
        &self.questions
    }

    /// Matches L against R on the first link of the folder, once R holds the server's tree and L
    /// the first scan's (section 6.4): every local entry R holds at the same path with the same
    /// kind and content is adopted. When `prior` shows that this device synced the space before,
    /// the others are held under one question about what was created on this device. Otherwise
    /// only those at a name R's trash holds are held, and the
    /// rest are sent as a Publish answer sends them, so a name the space holds gives them a
    /// conflict name without a question.
    pub fn first_link(
        &mut self,
        trees: &mut Trees,
        ids: &mut IdGen,
        now_ms: u64,
        prior: PriorSync,
    ) {
        let tops = first_link::adopt(trees, None);
        let (held, sent): (Vec<NodeId>, Vec<NodeId>) = match prior {
            PriorSync::Shown => (tops, Vec::new()),
            PriorSync::Fresh => tops.into_iter().partition(first_link::trashed_there(trees)),
        };
        self.published.extend(first_link::below(trees, &sent));
        if let Some(question) = first_link::ask(trees, held, None, None, ids, now_ms) {
            self.questions.get_mut().push(question);
        }
        self.hold(trees);
    }

    /// Stops committing for a recovery (section 6.4). The commit in flight is never sent again, and
    /// nothing is planned until [`Commits::recover`].
    pub fn enter_recovery(&mut self, cause: RecoveryCause) {
        self.halted.set(Some(Halt::Recovery(cause)));
    }

    /// Recovers once R holds the server's tree in the history the device recovers into, and L a
    /// scan of the disk (section 6.4). `device_seq` is the server's count of this device's commits
    /// there, which the next commit continues, and `history` the shapes the log gave nodes since
    /// the store's cursor, and `marks` the nodes the disk's entries name, by which an entry the store lost track of is found. The commit in flight is dropped: the tree changes it
    /// carried are still differences between L and S. Every node is classified by whether this
    /// device changed it since S. What is certain is left to the planner, and the rest is held, one
    /// question per group, with S kept as it was, so a held node is classified again against the
    /// same S whenever L or R changes. A restore asked for before the reset that R shows done ends
    /// as restored, and the others are held under one question.
    #[allow(clippy::too_many_arguments)]
    pub fn recover(
        &mut self,
        trees: &mut Trees,
        device_seq: u64,
        cause: &RecoveryCause,
        history: &History,
        marks: &Marks,
        ids: &mut IdGen,
        now_ms: u64,
    ) {
        self.halted.set(None);
        self.in_flight.set(None);
        self.accepted.set(device_seq);
        self.slots.clear();
        self.problems.clear();
        self.cap.set(None);
        self.split.clear();
        self.aliases.clear();
        self.published.clear();
        trees.rewound();
        (self.questions.get_mut())
            .retain(|question| matches!(question.kind, QuestionKind::RemoteMassDelete { .. }));
        let waiting = first_link::adopt_by_marks_first(trees, history, marks);
        let agreed: Vec<(NodeId, Option<Shape>)> = (trees.local().iter().map(|(id, _)| id))
            .chain(trees.synced().iter().map(|(id, _)| id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|id| !waiting.contains(*id))
            .filter_map(|id| {
                let local = trees.local().get(id).map(|node| &node.shape);
                let live = trees.remote().live(id);
                let last = trees.remote().get(id).map(|node| &node.shape);
                let shape = if same(local, live) {
                    live
                } else if local.is_some() && same(local, last) {
                    last
                } else {
                    return None;
                };
                Some((id.clone(), shape.cloned()))
            })
            .collect();
        for (id, shape) in &agreed {
            trees.sent_edit(id, shape.as_ref().and_then(|shape| shape.content.as_ref()));
        }
        let agreed = (agreed.into_iter()).filter(|(id, shape)| {
            !same(trees.synced().live(id), shape.as_ref()) && trees.waiting().get(id).is_none()
        });
        for (id, shape) in update_synced(trees, agreed.collect()) {
            trees.wait(id, shape, Origin::Other);
        }
        let mut groups = recovery::classify(trees);
        if let Some(created) = groups.remove(&RecoveryGroup::CreatedOnThisDevice) {
            let tops = (created.iter())
                .filter(|id| {
                    let parent = &trees.local().get(id).expect("created here").shape.parent;
                    !created.contains(parent)
                })
                .cloned();
            let question = first_link::ask(
                trees,
                tops.collect::<Vec<_>>(),
                None,
                Some(cause),
                ids,
                now_ms,
            );
            self.questions.get_mut().extend(question);
        }
        for (group, nodes) in groups {
            let question = recovery::ask(trees, group, &nodes, None, Some(cause), ids, now_ms);
            self.questions.get_mut().extend(question);
        }
        let done: Vec<RestoreId> = (self.restores.iter())
            .filter(|(_, intent)| recovery::done(trees, intent))
            .map(|(id, _)| id.clone())
            .collect();
        for id in done {
            self.resolve(id, Outcome::Restored);
        }
        let intents = recovery::ask_intents(self.restores.keys(), Some(cause), ids, now_ms);
        self.questions.get_mut().extend(intents);
        self.hold(trees);
    }

    /// Takes the person's answer to the open question `id`, routed by its kind. [`Answer::Dismiss`]
    /// closes a notice about other devices' deletions at once (section 6.1), and the guard never
    /// counts its deletions again. [`Answer::KeepBoth`] closes a conflict at once (section 2.7).
    /// [`Answer::KeepOriginal`] and [`Answer::KeepCopy`] stay on the conflict until a build carries
    /// them out ([`Commits::build`]). Any other answer stays on the question until the next build,
    /// which takes it only if the question still holds what it listed, and asks a new question
    /// otherwise.
    ///
    /// For the mass-delete guard's question (section 6.1), the next build counts what is pending
    /// first. [`Answer::Delete`] then makes the held deletions without counting them again.
    /// [`Answer::Restore`] clears S for every node of the question that is still deleted, so the
    /// effects download them again, and holds any other pending deletion under a new question.
    ///
    /// For a first link's question about entries created on this device (section 6.4),
    /// [`Answer::Publish`] lets the build send them as creates, and the server renames one whose
    /// name R holds. The person chose to keep both files then, so no conflict question is asked
    /// about it. [`Answer::Discard`] sets S to L for every entry, so that, R holding none of
    /// them, the effects move them to the local trash, and then download what R holds at their
    /// paths.
    ///
    /// An S update that would leave S invalid, such as for a folder whose child was moved out and
    /// is not acknowledged yet, waits ([`Trees::waiting`]). False, and nothing changes, when no
    /// open question has the ID, its kind does not take the answer, or it already took one.
    pub fn decide(&mut self, id: &str, answer: Answer) -> bool {
        let Some(index) = (self.questions.iter()).position(|question| {
            question.id == id
                && question.answer.is_none()
                && question.kind.answers().contains(&answer)
                && (matches!(
                    question.kind,
                    QuestionKind::MassDelete { .. }
                        | QuestionKind::RemoteMassDelete { .. }
                        | QuestionKind::Conflict { .. }
                        | QuestionKind::Recovery { .. }
                ))
        }) else {
            return false;
        };
        match self.questions[index].kind {
            QuestionKind::RemoteMassDelete { .. } => {
                self.questions.get_mut().remove(index);
                self.guard.dismiss_remote();
            }
            QuestionKind::Conflict { .. } if answer == Answer::KeepBoth => {
                self.questions.get_mut().remove(index);
            }
            _ => self.questions.get_mut()[index].answer = Some(answer),
        }
        true
    }

    /// Counts a removal that applied another device's deletion, as [`Effects::done`] returns it
    /// (section 6.1). When the removals of the window cross a limit of the guard, a notice opens.
    /// While it is open, each removal updates its counts and adds the limits crossed, and none
    /// leaves the window, until the person dismisses it. The rest of a deletion the dismissed
    /// notice counted, such as the files of a folder still being removed, is not counted.
    /// `last_seq` is the seq up to which R holds every change. The trees settle first, so the space
    /// counts the files S holds after the removal.
    ///
    /// [`Effects::done`]: crate::effects::Effects::done
    pub fn count_remote(
        &mut self,
        removal: RemoteRemoval,
        trees: &mut Trees,
        last_seq: u64,
        ids: &mut IdGen,
        now_ms: u64,
    ) {
        trees.settle();
        let open = (self.questions.iter())
            .position(|question| matches!(question.kind, QuestionKind::RemoteMassDelete { .. }));
        let noticed = open.is_some();
        let Some(window) = (self.guard).count_remote(removal, last_seq, noticed, &*trees, now_ms)
        else {
            return;
        };
        let mut exceeded = window.exceeded();
        let kind = |exceeded| QuestionKind::RemoteMassDelete {
            deletions: window.deletions,
            affected_files: window.affected_files,
            space_files: window.space_files,
            exceeded,
        };
        match open {
            Some(index) => {
                let notice = &mut self.questions.get_mut()[index];
                if let QuestionKind::RemoteMassDelete {
                    exceeded: before, ..
                } = &notice.kind
                {
                    exceeded.extend(before);
                    exceeded.sort();
                    exceeded.dedup();
                }
                notice.kind = kind(exceeded);
            }
            None if exceeded.is_empty() => {}
            None => self.questions.get_mut().push(Question {
                id: ids.uuid_v7(now_ms),
                asked_at_ms: now_ms,
                kind: kind(exceeded),
                held: BTreeMap::new(),
                answer: None,
                shown: BTreeMap::new(),
            }),
        }
    }

    fn ask_again(&mut self, trees: &mut Trees, ids: &mut IdGen, now_ms: u64) -> Vec<NodeId> {
        let shows = |question: &Question| first_link::asks(question) && !question.shown.is_empty();
        let mut left = Vec::new();
        let mut published_in: BTreeSet<NodeId> = BTreeSet::new();
        let mut index = 0;
        while index < self.questions.len() {
            if !shows(&self.questions[index]) {
                index += 1;
                continue;
            }
            let asked = &self.questions[index];
            let held = asked.holds().cloned().collect::<Vec<_>>();
            let question =
                first_link::ask(trees, held.clone(), Some(asked), asked.cause(), ids, now_ms);
            match (asked.cause(), &question) {
                (None, _) => {}
                (Some(_), Some(question)) => left.extend(
                    (held.into_iter()).filter(|id| !question.holds().any(|held| held == id)),
                ),
                (Some(_), None) => left.extend(held),
            }
            match question {
                Some(question) if question.answer.is_none() => {
                    self.questions.get_mut()[index] = question;
                    index += 1;
                }
                Some(question) => {
                    self.questions.get_mut().remove(index);
                    if question.answer == Some(Answer::Discard) {
                        let shown: BTreeSet<NodeId> = question.shown.keys().cloned().collect();
                        trees.discard(shown.iter().cloned(), &question.shown);
                        trees.discard_server_entries_below(&question.shown);
                        let updates = (question.shown.into_iter())
                            .map(|(id, shape)| (id, Some(shape)))
                            .collect();
                        for (id, shape) in update_synced(trees, updates) {
                            trees.wait(id, shape, Origin::Other);
                        }
                        trees.reclaim(|id| shown.contains(id));
                    } else {
                        for id in question.shown.keys() {
                            let mut at = trees.local().get(id).map(|node| &node.shape.parent);
                            while let Some(parent) =
                                at.filter(|p| published_in.insert((*p).clone()))
                            {
                                at = trees.local().get(parent).map(|node| &node.shape.parent);
                            }
                        }
                        self.published.extend(question.shown.into_keys());
                    }
                }
                None => {
                    self.questions.get_mut().remove(index);
                }
            }
        }
        self.hold(trees);
        trees.reclaim(|id| published_in.contains(id));
        left
    }

    /// Classifies the nodes the recovery questions hold again, against the same S (section 6.4).
    /// A node a certain case now applies to leaves its question, one whose group changed moves to
    /// that group's question, and a question that holds other nodes, or the same nodes at other
    /// revisions or shapes, is asked again under a new ID without its answer. A question that
    /// still holds what it showed is carried out when it has an answer, and returned: the commit
    /// that sends it carries the precondition that the head is still its base. No commit is in
    /// flight here, so an answer sent in one that the server refused or that was dropped is
    /// asked again.
    fn ask_recovery(
        &mut self,
        trees: &mut Trees,
        cause: Option<RecoveryCause>,
        left: Vec<NodeId>,
        ids: &mut IdGen,
        now_ms: u64,
    ) -> Vec<Question> {
        let mut asked: BTreeMap<RecoveryGroup, Question> = BTreeMap::new();
        let mut groups = recovery::regroup(trees, left);
        groups.remove(&RecoveryGroup::CreatedOnThisDevice);
        let mut intents = None;
        for mut question in std::mem::take(self.questions.get_mut()) {
            let QuestionKind::Recovery { group, sent_in, .. } = &mut question.kind else {
                self.questions.get_mut().push(question);
                continue;
            };
            let group = *group;
            if group == RecoveryGroup::CreatedOnThisDevice {
                self.questions.get_mut().push(question);
                continue;
            }
            if sent_in.take().is_some() {
                question.answer = None;
            }
            if group == RecoveryGroup::RequestedBeforeReset {
                intents = Some(question);
                continue;
            }
            for id in question.holds() {
                let Some(now) = recovery::group(trees, id) else {
                    continue;
                };
                let now = match now {
                    RecoveryGroup::CreatedOnThisDevice => group,
                    now => now,
                };
                groups.entry(now).or_default().insert(id.clone());
            }
            asked.entry(group).or_insert(question);
        }
        let owned: BTreeSet<NodeId> = (self.questions.iter())
            .filter(|question| first_link::asks(question))
            .flat_map(|question| question.held.values().flat_map(|item| item.members.keys()))
            .cloned()
            .collect();
        for nodes in groups.values_mut() {
            nodes.retain(|id| !owned.contains(id));
        }
        let mut updates = Vec::new();
        let mut released = Vec::new();
        for (group, nodes) in groups {
            let Some(question) = recovery::ask(
                trees,
                group,
                &nodes,
                asked.get(&group),
                cause.as_ref(),
                ids,
                now_ms,
            ) else {
                continue;
            };
            let Some(answer) = question.answer else {
                self.questions.get_mut().push(question);
                continue;
            };
            match answer {
                Answer::Discard => trees.discard(question.holds().cloned(), &question.shown),
                Answer::Publish | Answer::Keep => trees.approve(question.holds().cloned()),
                _ => {}
            }
            updates.extend(recovery::release(trees, &question, answer));
            if matches!(answer, Answer::Publish | Answer::Keep | Answer::Delete) {
                released.push(question);
            }
        }
        if let Some(question) = intents {
            let queued: Vec<RestoreId> = (question.held.keys())
                .filter(|id| self.restores.contains_key(*id))
                .cloned()
                .collect();
            match question.answer {
                Some(Answer::Cancel) => {
                    for id in queued {
                        self.resolve(id, Outcome::Rejected(Rejection::Cancelled));
                    }
                }
                Some(Answer::Repeat) if !queued.is_empty() => released.push(question),
                None if !queued.is_empty() => self.questions.get_mut().push(question),
                _ => {}
            }
        }
        for (id, shape) in update_synced(trees, updates) {
            trees.wait(id, shape, Origin::Other);
        }
        self.hold(trees);
        released
    }

    /// Keeps each answered question of `answered` for the members no acknowledged commit carried
    /// out yet: a node a group still holds, or a restore still queued. Its answer stays bound to
    /// what the person was shown, so the next build sends them only while that still holds.
    fn keep_unsent(&mut self, trees: &Trees, answered: Vec<Question>) {
        for question in answered {
            let intents = matches!(
                question.kind,
                QuestionKind::Recovery {
                    group: RecoveryGroup::RequestedBeforeReset,
                    ..
                }
            );
            let kept = |id: &str| match intents {
                true => self.restores.contains_key(id),
                false => recovery::group(trees, id).is_some(),
            };
            let kept = recovery::narrow(trees, question, kept);
            self.questions.get_mut().extend(kept);
        }
    }

    /// Tells the trees which local entries the open questions hold ([`Question::shown`],
    /// [`Trees::set_asked`]). The planner does whenever its questions change; a caller that
    /// restores it from a state calls this.
    pub fn hold(&self, trees: &mut Trees) {
        let shown = (self.questions.iter()).flat_map(|question| {
            let held = recovery::asks(question).then(|| question.holds());
            question.shown.keys().chain(held.into_iter().flatten())
        });
        trees.set_asked(shown.cloned().collect());
    }

    fn resolve(&mut self, id: RestoreId, outcome: Outcome) {
        let intent = self
            .restores
            .get_mut()
            .remove(&id)
            .expect("a queued restore");
        self.resolved.get_mut().push(Resolved {
            id,
            intent,
            outcome,
        });
        let excess = self.resolved.len().saturating_sub(RESOLVED_KEPT);
        self.resolved.get_mut().drain(..excess);
    }

    /// On a read-only device nothing is committed, so a queued restore would wait without an
    /// end. Each ends as rejected, except those the commit in flight carries: that commit may
    /// have been applied, and its result decides them.
    fn reject_unsent_when_read_only(&mut self) {
        if *self.halted != Some(Halt::ReadOnly) {
            return;
        }
        let ops = self.in_flight.as_ref().map(|flight| &flight.commit.ops[..]);
        let sent: BTreeSet<RestoreId> = carried(&self.restores, ops.unwrap_or_default())
            .expect("a Restore op in flight is queued")
            .into_values()
            .collect();
        let unsent: Vec<RestoreId> = (self.restores.keys())
            .filter(|id| !sent.contains(*id))
            .cloned()
            .collect();
        for id in unsent {
            self.resolve(id, Outcome::Rejected(Rejection::ReadOnly));
        }
    }

    /// Builds the next commit from local changes, unless one is in flight or committing has
    /// stopped. While committing has stopped it still makes the waiting S updates that fit (see
    /// [`Trees::waiting`]), so the effects plan their nodes again as downloads go on. The caller
    /// builds only when connected and caught up, with `base` the `last_seq` of R and its commit
    /// token, and `history_id` the history R belongs to. A resend keeps the token, so a server that
    /// lost the commit at `base_seq` refuses it (RFC 0013).
    ///
    /// Concurrent changes are rebased onto R first (section 2.4): a file whose content changed
    /// differently on both sides is forked, its local version becoming a new node that is created
    /// under the original name; a node changed remotely keeps the nodes L deleted on its way to the
    /// root, which are downloaded again instead of deleted. Then every local change becomes ops,
    /// ordered on a scratch copy of R (section 2.6): an op is emitted once it is valid there. When
    /// none is, moves that each wait for a name another one frees, as in a swap, go through a
    /// pending name: one node of the cycle moves to a free name first and to its own name last, in
    /// the same commit when the commit holds the whole cycle. Otherwise a create or move that only
    /// waits for a name is sent anyway, for the server to rename, except that a folder created
    /// where a moving file or folder still is takes a pending name first. A folder is not deleted
    /// after a folder created beside it, or beside a folder the created folder's parent may be
    /// merged into, since another device may have renamed it to the created folder's name since R,
    /// and the server would then merge the created folder into it: a delete that can go is sent
    /// before the create, and one that cannot waits for the next commit. A delete waits until
    /// nothing that survives is
    /// left under it, a create or move until its parent exists, and a move that would close a cycle
    /// against R is reverted: the node is marked in [`Trees::reverted`], and the effects move it
    /// back to its R position on disk, bringing back the folders L deleted on its way. A parked or
    /// reverted node waits until the effects have moved it, and so does a node with a problem or a
    /// waiting S update, except a parked node whose name is held by a node the effects do not park:
    /// it is sent for the server to rename. A parked node that neither R nor S holds, such as a
    /// file forked from a parked node R no longer has, is moved back to the position L keeps for it
    /// first; while the effects cannot move it there, because another entry of L holds that name or
    /// L no longer holds its folder, it is created from that position while still parked, and the
    /// effects then move it to the position R gives it. A commit stops at [`MAX_OPS`], at
    /// [`MAX_COMMIT_BYTES`] of estimated change records, or at half the size of the last commit
    /// that did not fit. The rebase changes the trees even when no commit results, so the caller
    /// plans effects again after building.
    ///
    /// The queued restores come first, in request order, so local changes of other nodes do not
    /// hold them back. A restore follows every op this commit has for its own node: an unsent
    /// edit of a file is sent before the restore of an earlier version and so becomes a version
    /// itself. While such an op is left for a later commit, the restore waits with it. It also
    /// waits while its node is not settled: a node left out of planning (a problem node, a node
    /// whose S update waits and that L did not change, a parked one),
    /// and a node L and R agree on while S lags ([`Class::Same`]). A folder is not deleted in the
    /// commit that restores a node into it.
    ///
    /// An excluded node ([`Trees::excluded`]) gets no op and is not forked, and nothing is built
    /// while the rules are unknown ([`Trees::rules_known`]). A folder L deleted that still holds
    /// an excluded node in R is not deleted, since that would trash the excluded node on the
    /// server: each topmost deleted node below it with no excluded node in its subtree gets its
    /// own delete, and the folder stays (section 5.10).
    ///
    /// A node an open question holds gets no op. Every other deletion that could be planned now goes
    /// through the mass-delete guard first (section 6.1), and the commit carries the changes it
    /// does not hold.
    pub fn build(
        &mut self,
        trees: &mut Trees,
        ids: &mut IdGen,
        now_ms: u64,
        history_id: &str,
        device_id: &str,
        base: impl Into<Base>,
    ) -> Option<&proto::Commit> {
        let base = base.into();
        if self.in_flight.is_some() {
            return None;
        }
        if self.halted.is_none() {
            trees.resend_folders_of_changes();
        }
        trees.unsend_finishes(None);
        trees.applied_head(base.seq);
        trees.check_finishes();
        self.retry_waiting(trees, BTreeSet::new(), Vec::new());
        if self.halted.is_some() || !trees.rules_known() {
            return None;
        }
        let held: BTreeSet<NodeId> = self
            .questions
            .iter()
            .flat_map(Question::holds)
            .cloned()
            .collect();
        first_link::adopt_equal_files(trees, &held);
        let cause = self.questions.iter().find_map(Question::cause).cloned();
        let left = self.ask_again(trees, ids, now_ms);
        let released = self.ask_recovery(trees, cause, left, ids, now_ms);
        let reclaimed = trees.reclaim(|_| true);
        let discardable = reclaimed
            .into_iter()
            .filter(|id| !holds_server_entry(trees, id));
        if let Some(question) =
            first_link::ask(trees, discardable, None, cause.as_ref(), ids, now_ms)
        {
            self.questions.get_mut().push(question);
            self.hold(trees);
        }
        let waiting = (trees.waiting().keys()).filter(|id| !changed_locally(trees, id));
        let mut skip: BTreeSet<NodeId> = self
            .problems
            .iter()
            .chain(waiting)
            .chain(trees.held_finishes())
            .chain(parked_until_moved(trees))
            .chain(
                (self.questions.iter())
                    .filter(|q| recovery::asks(q) || first_link::asks(q))
                    .flat_map(Question::holds),
            )
            .cloned()
            .collect();
        let (needed, agreed) = rebase(trees, ids, now_ms, &skip);
        skip.extend(parked_until_moved(trees).cloned());
        let excluded: Vec<NodeId> = (trees.dirty_others().iter())
            .filter(|id| trees.excluded(id) || trees.held(id))
            .cloned()
            .collect();
        skip.extend(excluded);
        let needed = self.retry_waiting(trees, needed, agreed);
        let cap = self.cap.unwrap_or(MAX_OPS);
        let (asking, answered): (Vec<String>, Vec<proto::Op>) = self
            .conflicts(trees, &skip, cap, ids, now_ms)
            .into_iter()
            .unzip();
        skip.extend(
            (answered.iter().filter_map(|op| op.op.as_ref())).map(|op| op_node(op).to_owned()),
        );
        let guarded =
            |question: &Question| matches!(question.kind, QuestionKind::MassDelete { .. });
        let index = self.questions.iter().position(guarded);
        let mut asked = index.map(|index| self.questions.get_mut().remove(index));
        let open = asked.as_ref().map(|question| question.id.clone());
        skip.extend(self.questions.iter().flat_map(Question::holds).cloned());
        let deleted = (trees.dirty_others().iter())
            .filter(|id| !skip.contains(*id) && deletes(trees, id, &needed))
            .cloned()
            .collect();
        let pending = guard::deletions(trees, &deleted);
        let checked = (self.guard).check(&mut asked, pending, trees, ids, now_ms);
        skip.extend(checked.held);
        let restored: Vec<NodeId> = (checked.restored.into_values())
            .flat_map(|held| held.members.into_keys())
            .collect();
        skip.extend(restored.iter().cloned());
        let updates = restored.into_iter().map(|id| (id, None)).collect();
        for (id, shape) in update_synced(trees, updates) {
            trees.wait(id, shape, Origin::Other);
        }
        if let Some(question) = asked {
            let kept = index.filter(|_| open.as_ref() == Some(&question.id));
            let at = kept.unwrap_or(self.questions.len());
            self.questions.get_mut().insert(at, question);
        }
        self.hold(trees);
        self.split.retain(|id| trees.synced().live(id).is_some());
        self.published.retain(|id| trees.local().get(id).is_some());
        self.aliases
            .retain(|id, _| trees.local().get(id).is_some() && trees.reverted().contains(id));
        let intents = RecoveryGroup::RequestedBeforeReset;
        let held_restores: BTreeSet<&RestoreId> = (self.questions.iter())
            .filter(|question| matches!(question.kind, QuestionKind::Recovery { group, .. } if group == intents))
            .flat_map(|question| question.held.keys())
            .collect();
        let held_ops: Vec<&proto::Restore> = (self.restores.iter())
            .filter(|(id, _)| held_restores.contains(id))
            .map(|(_, intent)| intent.op())
            .collect();
        let restores = (self.restores.iter())
            .filter(|(_, intent)| !held_ops.contains(&intent.op()))
            .map(|(id, intent)| (id.clone(), intent.clone()))
            .collect();
        let answers = released.iter().flat_map(Question::holds).cloned().collect();
        let (creates, ready) = (trees.dirty_creates(), trees.ready_creates());
        let aliased: Vec<&NodeId> = (self.aliases.keys())
            .flat_map(|alias| trees.local_children(alias))
            .filter(|id| creates.contains(*id) && !ready.contains(*id))
            .collect();
        let mut window = 2 * cap;
        let (planned, deletes, reverts, cycle_reverts) = loop {
            let mut seeds = ready.iter().peekable();
            let mut next: BinaryHeap<Reverse<(usize, &NodeId)>> =
                aliased.iter().map(|id| Reverse((1, *id))).collect();
            let mut picked = Vec::new();
            let mut bound = None;
            loop {
                let seeded = match (seeds.peek(), next.peek()) {
                    (Some(id), Some(Reverse(top))) => (1, *id) < *top,
                    (seed, _) => seed.is_some(),
                };
                let popped = match seeded {
                    true => seeds.next().map(|id| (1, id)),
                    false => next.pop().map(|Reverse(key)| key),
                };
                let Some((pass, id)) = popped else {
                    break;
                };
                if trees.excluded(id) || trees.held(id) {
                    continue;
                }
                if picked.len() == window {
                    bound = Some((pass, id.as_str()));
                    break;
                }
                picked.push(id);
                if !self.aliases.contains_key(id) {
                    next.extend(
                        (trees.local_children(id))
                            .filter(|child| creates.contains(*child))
                            .map(|child| Reverse((pass + usize::from(child < id), child))),
                    );
                }
            }
            let planned_from = |picked: Vec<&NodeId>, bound| {
                let mut candidates: Vec<&NodeId> = (picked.into_iter())
                    .filter(|id| !trees.excluded(id) && !trees.held(id))
                    .chain(trees.dirty_others())
                    .collect();
                candidates.sort_unstable();
                plan(
                    trees,
                    &candidates,
                    bound,
                    &skip,
                    &self.problems,
                    &needed,
                    cap,
                    &answered,
                    &self.split,
                    &self.aliases,
                    &restores,
                    &answers,
                    device_id,
                )
            };
            let (planned, exact) = planned_from(picked, bound);
            if exact {
                debug_assert!(
                    bound.is_none()
                        || creates.len() > 512
                        || planned == planned_from(creates.iter().collect(), None).0,
                    "a windowed plan commits what a plan of every create commits"
                );
                break planned;
            }
            window *= 4;
        };
        for id in reverts {
            trees.revert(id);
        }
        for id in cycle_reverts {
            trees.revert_for_cycle(id);
        }
        for op in &planned {
            let Some(op::Op::Move(moved)) = &op.op else {
                continue;
            };
            let owed = (trees.remote().get(&moved.node_id)).is_some_and(|remote| {
                remote.pending_by.as_deref() == Some(device_id)
                    && remote.shape.parent == moved.new_parent_id
                    && (remote.pending_for.as_deref())
                        .is_some_and(|mark| nfc(mark) == nfc(&moved.new_name))
            });
            if owed {
                trees.start_finish(&moved.node_id);
            }
        }
        let finish = |trees: &Trees, op: &proto::Op| match &op.op {
            Some(op::Op::Move(moved)) => (trees.pending_finishes().get(&moved.node_id))
                .filter(|finish| {
                    finish.parent == moved.new_parent_id
                        && nfc(&finish.name) == nfc(&moved.new_name)
                })
                .map(|_| moved.node_id.clone()),
            _ => None,
        };
        let mut ops: Vec<proto::Op> = planned.into_iter().chain(answered).collect();
        if ops.is_empty() && released.is_empty() {
            let mut bytes = 0;
            let first: Vec<(NodeId, NodeId, String)> = (finishes(trees, &skip, Some(device_id)))
                .filter(|(id, .., free)| !free || disk_first(trees, id))
                .take(cap)
                .take_while(|(id, _, name, _)| {
                    let shape = trees.remote().live(id).or(trees.synced().live(id));
                    let cost = record_cost(shape) + name.len();
                    let fits = bytes == 0 || bytes + cost <= MAX_COMMIT_BYTES;
                    bytes += cost;
                    fits
                })
                .map(|(id, parent, name, _)| (id.clone(), parent.clone(), name.to_owned()))
                .collect();
            for (id, parent, name) in first {
                trees.start_finish(&id);
                let pending = trees
                    .remote()
                    .get(&id)
                    .expect("a node R marks")
                    .shape
                    .name
                    .clone();
                ops.push(proto::Op {
                    op: Some(op::Op::Move(proto::Move {
                        from: Some(proto::Place {
                            parent_id: parent.clone(),
                            name: pending,
                        }),
                        node_id: id,
                        new_parent_id: parent,
                        new_name: name,
                        pending_for: String::new(),
                    })),
                });
            }
        }
        if ops.is_empty() {
            self.keep_unsent(trees, released);
            return None;
        }
        let finishing: Vec<NodeId> = ops.iter().filter_map(|op| finish(trees, op)).collect();
        for id in &finishing {
            trees.send_finish(id);
        }
        trees.send_revivals(
            &(ops.iter().filter_map(|op| op.op.as_ref()))
                .map(|op| op_node(op).to_owned())
                .collect(),
        );
        trees.watch_content(ops.iter().filter_map(|op| match &op.op {
            Some(op::Op::CreateFile(create)) => Some(create.node_id.clone()),
            Some(op::Op::UpdateContent(update)) => Some(update.node_id.clone()),
            _ => None,
        }));
        trees.watch_positions(
            (ops.iter()
                .filter_map(|op| op.op.as_ref().and_then(positioned)))
            .map(ToOwned::to_owned),
        );

        let require_head = (finishing.iter()).any(|id| {
            trees.pending_finishes()[id].origin == FinishOrigin::Legacy
                || (trees.remote().get(id)).is_none_or(|remote| remote.pending_for.is_none())
        }) || !released.is_empty()
            || (ops.iter().filter_map(|op| op.op.as_ref()))
                .any(|op| trees.resends().contains(op_node(op)));
        let restores = (ops.iter().enumerate())
            .filter_map(|(index, op)| match &op.op {
                Some(op::Op::Restore(restore)) => Some((index, restore)),
                _ => None,
            })
            .map(|(index, restore)| {
                let removed = trees.removed_restored(&restore.node_id, &restore.target_parent_id);
                (index, removed)
            })
            .filter(|(_, removed)| !removed.is_empty())
            .collect();
        let commit_id = ids.uuid_v7(now_ms);
        for mut question in released {
            if let QuestionKind::Recovery { sent_in, .. } = &mut question.kind {
                *sent_in = Some(commit_id.clone());
            }
            self.questions.get_mut().push(question);
        }
        for question in self.questions.get_mut() {
            if let QuestionKind::Conflict { sent_in, .. } = &mut question.kind
                && asking.contains(&question.id)
            {
                *sent_in = Some(commit_id.clone());
            }
        }
        self.in_flight.set(Some(InFlight {
            commit: proto::Commit {
                commit_id,
                base_seq: base.seq,
                base_commit_id: base.commit_id,
                base_committed_at_ms: base.committed_at_ms,
                ops,
                history_id: history_id.to_owned(),
                device_seq: *self.accepted + 1,
                require_head,
                ..Default::default()
            },
            deletes,
            restores,
            result: None,
        }));
        self.in_flight.as_ref().map(|flight| &flight.commit)
    }

    fn conflicts(
        &mut self,
        trees: &Trees,
        skip: &BTreeSet<NodeId>,
        cap: usize,
        ids: &mut IdGen,
        now_ms: u64,
    ) -> Vec<(String, proto::Op)> {
        let mut ops = Vec::new();
        let mut kept = Vec::new();
        let mut taken = BTreeSet::new();
        for mut question in std::mem::take(self.questions.get_mut()) {
            let QuestionKind::Conflict { original, copy, .. } = &mut question.kind else {
                kept.push(question);
                continue;
            };
            match drift(trees, original, copy) {
                Drift::Ended => continue,
                Drift::Content => {
                    for file in [&mut *original, &mut *copy] {
                        let remote = trees.remote().get(&file.node_id).expect("live in R");
                        file.revisions.content_rev = remote.content_rev;
                    }
                    question.id = ids.uuid_v7(now_ms);
                    question.asked_at_ms = now_ms;
                    question.answer = None;
                    kept.push(question);
                    continue;
                }
                Drift::None => {}
            }
            let ready = in_sync(trees, original, copy)
                && !skip.contains(&original.node_id)
                && !skip.contains(&copy.node_id);
            let free = !taken.contains(&original.node_id) && !taken.contains(&copy.node_id);
            let content = |file: &ConflictFile| {
                let remote = trees.remote().get(&file.node_id).expect("live in R");
                remote.shape.content.clone()
            };
            let held = trees.remote().get(&original.node_id).expect("live in R");
            let delete_copy = op::Op::Delete(proto::Delete {
                node_id: copy.node_id.clone(),
                requires: Some(proto::NodeRevisions {
                    node_id: original.node_id.clone(),
                    content_rev: held.content_rev,
                    meta_rev: held.meta_rev,
                }),
            });
            let op = match question.answer {
                Some(Answer::KeepOriginal) => Some(delete_copy),
                Some(Answer::KeepCopy) => match (content(original), content(copy)) {
                    (Some(current), Some(copied)) if same_content(&current, &copied) => {
                        Some(delete_copy)
                    }
                    (Some(_), Some(copied))
                        if copied.chunks.len() == copied.chunk_count as usize =>
                    {
                        Some(op::Op::UpdateContent(proto::UpdateContent {
                            node_id: original.node_id.clone(),
                            content: Some(copied),
                            resend: false,
                        }))
                    }
                    _ => {
                        question.answer = None;
                        None
                    }
                },
                _ => None,
            };
            if let Some(op) = op
                && ready
                && free
                && ops.len() < cap
            {
                taken.insert(original.node_id.clone());
                taken.insert(copy.node_id.clone());
                ops.push((question.id.clone(), proto::Op { op: Some(op) }));
            }
            kept.push(question);
        }
        self.questions.set(kept);
        ops
    }

    /// Whether a build has something to do: a node is dirty or its S update waits, a restore is
    /// queued, a conflict question is due ([`Commits::conflicts_due`]), or a node R marks with a
    /// pending name may take that name (RFC 0017).
    pub fn due(&self, trees: &Trees, device_id: &str) -> bool {
        !trees.dirty().is_empty()
            || !trees.waiting().is_empty()
            || !self.restores.is_empty()
            || self.conflicts_due(trees)
            || finishes(trees, &self.problems, Some(device_id))
                .any(|(id, .., free)| !free || disk_first(trees, id))
    }

    /// Whether a build has something to do for a conflict question: a file of it was renamed,
    /// moved, deleted or changed since it was asked, or its answer can be carried out
    /// ([`Commits::conflicts`]). The caller builds then even when nothing else changed.
    pub fn conflicts_due(&self, trees: &Trees) -> bool {
        self.questions.iter().any(|question| match &question.kind {
            QuestionKind::Conflict { original, copy, .. } => {
                drift(trees, original, copy) != Drift::None
                    || (question.answer.is_some() && in_sync(trees, original, copy))
            }
            _ => false,
        })
    }

    /// The ID of the conflict question whose answer `op` of commit `commit_id` carries out
    /// ([`Commits::conflicts`]): the commit was built with it, and the op is its `Delete` of the copy
    /// or `UpdateContent` of the original. Such an op leaves S as it is when it is acknowledged,
    /// and its node is not marked as a problem. Another op for either file, such as the upload of
    /// an edit, is acknowledged as any op is.
    fn answer_for(&self, commit_id: &str, op: &op::Op) -> Option<String> {
        let question = self.questions.iter().find(|question| {
            let QuestionKind::Conflict {
                original,
                copy,
                sent_in,
            } = &question.kind
            else {
                return false;
            };
            sent_in.as_deref() == Some(commit_id)
                && match op {
                    op::Op::Delete(delete) => delete.node_id == copy.node_id,
                    op::Op::UpdateContent(update) => update.node_id == original.node_id,
                    _ => false,
                }
        });
        question.map(|question| question.id.clone())
    }

    /// Asks about the conflict between the files `original` and `copy` that op `index` of commit
    /// `commit_id` made, unless one of them is not a live file in R or it is already asked. The
    /// question's ID comes from the op, so acknowledging the commit again asks the same question,
    /// and it is asked when the commit was built.
    fn ask_conflict(
        &mut self,
        trees: &Trees,
        commit_id: &str,
        index: usize,
        original: NodeId,
        copy: NodeId,
    ) {
        let file = |id: NodeId| {
            let node = trees.remote().get(&id)?;
            (!node.trashed && node.shape.kind == Kind::File).then_some(ConflictFile {
                node_id: id,
                revisions: Revisions {
                    content_rev: node.content_rev,
                    meta_rev: node.meta_rev,
                },
            })
        };
        let (Some(original), Some(copy)) = (file(original), file(copy)) else {
            return;
        };
        let asked = self.questions.iter().any(|question| {
            matches!(&question.kind, QuestionKind::Conflict { original: o, copy: c, .. }
                if o.node_id == original.node_id && c.node_id == copy.node_id)
        });
        if asked {
            return;
        }
        self.questions.get_mut().push(Question {
            id: format!("{commit_id}-{index}"),
            asked_at_ms: unix_ms(commit_id).unwrap_or_default(),
            kind: QuestionKind::Conflict {
                original,
                copy,
                sent_in: None,
            },
            held: BTreeMap::new(),
            answer: None,
            shown: BTreeMap::new(),
        });
    }

    /// Keeps the result of the commit in flight until it can be acknowledged.
    pub fn received(&mut self, result: proto::CommitResult) -> Result<(), CommitError> {
        let flight = self
            .in_flight
            .get_mut()
            .as_mut()
            .ok_or(CommitError::NotInFlight)?;
        if result.results.len() != flight.commit.ops.len() {
            return Err(CommitError::ResultMismatch);
        }
        flight.result = Some(result);
        Ok(())
    }

    /// Acknowledges the commit in flight once R holds its result's records, that is once
    /// `last_seq` has reached the result's seq (section 2.7): the caller applies a result whose
    /// seq is `last_seq + 1` to R first, and a result further ahead waits for the commits before
    /// it. S takes what was sent, not what the server returned, so any difference shows up as a
    /// remote change; a create or move the server renamed takes the server's name instead, and the
    /// node is reverted so that the disk takes it too. Content S took from R after the commit was
    /// built is newer than what was sent: the server applies a content update only when no other
    /// device changed the content after the commit's `base_seq`, and a created file has no content
    /// before its create, so that change came after the commit. When S's content changed since then ([`Trees::took_content`]), even back to what it
    /// was, S keeps what it holds. A position is newer than the one a create or move sent only when L
    /// and R agreed on it, after the commit was built, at a revision of R after the result's seq,
    /// whether or not S took it: the server applies a move whatever another device did after
    /// `base_seq`, so the move replaced a position from an earlier record, and a position only R
    /// came back to is a remote change. When L and R agreed on a newer position
    /// ([`Trees::position_after`]), S takes that position, and a rename by the server does not
    /// revert the node. S first takes what every op sent, as without an agreement, and then each
    /// agreed position S can hold, with the rest of the shape the ops gave the node; agreed positions
    /// that only fit together are made together ([`update_synced`]). A node whose agreed position S
    /// cannot hold, because another node holds its name or its parent is gone, keeps what was sent,
    /// and the agreed position waits as its S update while L or R still holds it there, so the node
    /// is not planned until S can hold it. A node whose agreement does not wait is reverted when the
    /// server renamed it. A conflict copy is re-keyed in L, and so is
    /// a directory merged into one L does not hold yet. When L already holds the conflict copy,
    /// because a log replay brought it before the result and the effects downloaded it, the local
    /// file keeps its ID and takes what was sent as its S, so the server's content replaces it. When L already holds the directory it was
    /// merged into, the created directory and the nodes created in it are reverted: on disk they
    /// move into the merged directory and the emptied one is removed. The accepted counter takes
    /// the commit's slot. Returns false while there is nothing to acknowledge.
    ///
    /// A create the server refuses with ID_EXISTS while R holds the node, live or in the trash, is
    /// no problem. Only another device can have created a node under this device's ID first, as
    /// when both send a file the server lost again (section 4.4), and it may have deleted the node
    /// since. S stays without a node R holds live, so the next plan compares L with what R holds
    /// instead of sending the create again: equal content leaves nothing to send, and other
    /// content is kept beside R's as a new node (section 2.4). A node R holds in the trash keeps
    /// the S that L and R agreed on while the create was out, and otherwise takes R's content at the
    /// position the create sent as its S. So L follows the delete unless it holds content the
    /// server never showed for the node, which revives it (R8).
    ///
    /// A `Restore` op changes neither S nor L. What it sent is an instruction, and only the server
    /// knows the nodes and the content it brought back, so all of it is a remote change that R
    /// already holds and the effects apply. The restore leaves the queue with its outcome,
    /// whatever the result, and its node is never marked as a problem.
    ///
    /// S stays a valid tree. When the updates together would break it, because S still lags R
    /// elsewhere, each is made as soon as it fits and the rest wait (see [`Trees::waiting`]).
    ///
    /// Once R holds the record of the commit in flight and its result has not arrived
    /// ([`Commits::own_record`]), what the record shows of the commit is recorded as the result
    /// would record it: the nodes its deletes trashed and the positions its creates and moves set,
    /// as its edits are recorded when the record arrives ([`Commits::edits_in_flight`]). A loss
    /// that drops the commit before its result then sends those changes again as this device's own
    /// ([`Trees::forget_lost`]).
    pub fn acknowledge(&mut self, trees: &mut Trees, last_seq: u64) -> bool {
        let recovering = matches!(*self.halted, Some(Halt::Recovery(_)));
        if !recovering {
            self.record_served(trees, last_seq);
        }
        !recovering && self.acknowledge_flight(trees, last_seq)
    }

    /// The nodes the server gave to the creates, merged folders and conflict copies of each commit acknowledged
    /// since the last call ([`given_nodes`]), for the disk to mark on the entries L holds under them.
    pub fn take_granted(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.granted.0)
    }

    fn record_served(&self, trees: &mut Trees, last_seq: u64) {
        let Some(flight) = (self.in_flight.as_ref()).filter(|flight| flight.result.is_none())
        else {
            return;
        };
        let Some(&seq) =
            (self.slots.get(&flight.commit.device_seq)).filter(|&&seq| seq <= last_seq)
        else {
            return;
        };
        for op in flight.commit.ops.iter().filter_map(|op| op.op.as_ref()) {
            match op {
                op::Op::Delete(delete) => {
                    let kept = delete.requires.as_ref().map(|kept| kept.node_id.as_str());
                    trees.record_trashed(&delete.node_id, seq, kept);
                    for id in (flight.deletes.get(&delete.node_id).into_iter().flatten())
                        .filter(|id| **id != delete.node_id)
                    {
                        trees.record_trashed(id, seq, None);
                    }
                }
                op::Op::CreateDir(_) | op::Op::CreateFile(_) | op::Op::Move(_) => {
                    trees.record_moved(op_node(op), seq);
                }
                op::Op::UpdateContent(_) | op::Op::Restore(_) => {}
            }
        }
    }

    fn acknowledge_flight(&mut self, trees: &mut Trees, last_seq: u64) -> bool {
        trees.applied_head(last_seq);
        let Some(flight) = self
            .in_flight
            .take_if(|flight| flight.result.as_ref().is_some_and(|r| r.seq <= last_seq))
        else {
            return false;
        };
        let result = flight.result.expect("taken with a result");
        (self.granted.0).extend(
            given_nodes(&flight.commit, &result)
                .into_iter()
                .map(|(_, node)| node),
        );
        let commit_id = &flight.commit.commit_id;
        let (sent, questions): (Vec<Question>, Vec<Question>) = std::mem::take(self.questions.get_mut()).into_iter().partition(|question| {
            matches!(&question.kind, QuestionKind::Recovery { sent_in: Some(sent), .. } if sent == commit_id)
        });
        self.questions.set(questions);
        let mut synced = Synced::default();
        let mut merged: BTreeMap<NodeId, NodeId> = BTreeMap::new();
        let mut agreed: BTreeMap<NodeId, Agreed> = BTreeMap::new();
        let mut unreverted: BTreeSet<NodeId> = BTreeSet::new();
        let aliases = self.aliases.clone();
        let lost: Vec<NodeId> = (flight.commit.ops.iter())
            .filter_map(|op| match &op.op {
                Some(op::Op::CreateDir(create)) => Some(&create.node_id),
                Some(op::Op::CreateFile(create)) => Some(&create.node_id),
                _ => None,
            })
            .filter(|id| trees.lost_position(id).is_some())
            .cloned()
            .collect();
        let mut conflicts: Vec<(usize, NodeId, NodeId)> = Vec::new();
        let mut answers = Vec::new();
        let revivers: BTreeSet<&str> = (flight.commit.ops.iter())
            .filter_map(|op| match &op.op {
                Some(op::Op::Move(moved)) => Some(moved.node_id.as_str()),
                Some(op::Op::Restore(restore)) => Some(restore.node_id.as_str()),
                _ => None,
            })
            .collect();
        let placed: BTreeSet<&NodeId> = (flight.commit.ops.iter())
            .filter_map(|op| match &op.op {
                Some(op::Op::Move(moved)) => Some(&moved.node_id),
                _ => None,
            })
            .collect();
        for (index, (op, outcome)) in flight.commit.ops.iter().zip(&result.results).enumerate() {
            use op_result::Code;
            let code = Code::try_from(outcome.code).unwrap_or(Code::Unspecified);
            if let Some(sent) = &op.op
                && let Some(id) = self.answer_for(&flight.commit.commit_id, sent)
            {
                answers.push((id, index, sent, code, &outcome.node_id));
                continue;
            }
            let applied = matches!(code, Code::Applied | Code::AppliedRenamed);
            let taken = code == Code::Rejected && outcome.reject_reason == "ID_EXISTS";
            let parent = |sent: &str| merged.get(sent).cloned().unwrap_or_else(|| sent.to_owned());
            let renamed = code == Code::AppliedRenamed && !outcome.name.is_empty();
            let name = |sent: &str| match renamed {
                true => outcome.name.clone(),
                false => sent.to_owned(),
            };
            let sent = match &op.op {
                Some(op::Op::CreateDir(create)) => Some((&create.parent_id, &create.name)),
                Some(op::Op::CreateFile(create)) => Some((&create.parent_id, &create.name)),
                Some(op::Op::Move(moved)) => Some((&moved.new_parent_id, &moved.new_name)),
                _ => None,
            };
            let (node, done) = match &op.op {
                Some(op::Op::CreateDir(create)) => {
                    let shape = Shape {
                        parent: parent(&create.parent_id),
                        name: name(&create.name),
                        kind: Kind::Dir,
                        content: None,
                    };
                    let into = &outcome.node_id;
                    let (trashed, moved_in): (Vec<NodeId>, Vec<(NodeId, Option<Shape>)>) =
                        match code == Code::MergedInto {
                            false => Default::default(),
                            true => {
                                let (trashed, moved_in): (Vec<&NodeId>, Vec<&NodeId>) = (trees
                                    .local_children(&create.node_id))
                                .filter(|id| {
                                    !placed.contains(*id)
                                        && (synced.get(trees, id))
                                            .is_some_and(|shape| shape.parent != create.node_id)
                                })
                                .partition(|id| {
                                    trees.remote().get(id).is_some_and(|remote| remote.trashed)
                                });
                                let moved_in = (moved_in.into_iter())
                                    .map(|id| {
                                        let position = (trees.remote().live(id))
                                            .zip(synced.get(trees, id))
                                            .map(|(remote, synced)| Shape {
                                                parent: remote.parent.clone(),
                                                name: remote.name.clone(),
                                                ..synced
                                            });
                                        (id.clone(), position)
                                    })
                                    .collect();
                                (trashed.into_iter().cloned().collect(), moved_in)
                            }
                        };
                    if code == Code::MergedInto && trees.local().get(into).is_some() {
                        trees.revert(create.node_id.clone());
                        merged.insert(create.node_id.clone(), into.clone());
                        self.aliases.insert(create.node_id.clone(), into.clone());
                    } else if code == Code::MergedInto {
                        let lost = trees.lost_position(&create.node_id).is_some();
                        rekey(trees, &create.node_id, into);
                        let held: Vec<(NodeId, Shape)> = (trees.synced().iter())
                            .map(|(id, _)| id)
                            .chain(trees.waiting().keys())
                            .filter_map(|id| Some((id.clone(), synced.get(trees, id)?)))
                            .filter(|(_, shape)| shape.parent == create.node_id)
                            .collect();
                        for (id, shape) in held {
                            synced.set(
                                &id,
                                Some(Shape {
                                    parent: into.clone(),
                                    ..shape
                                }),
                            );
                        }
                        trees.rekey_lost_positions(&create.node_id, into);
                        if lost {
                            trees.take_lost_position(
                                into.clone(),
                                shape,
                                Origin::Commit(flight.commit.device_seq),
                            );
                        } else {
                            synced.set(into, Some(shape));
                        }
                        merged.insert(create.node_id.clone(), into.clone());
                    } else if applied && trees.lost_position(&create.node_id).is_some() {
                        trees.take_lost_position(
                            create.node_id.clone(),
                            shape,
                            Origin::Commit(flight.commit.device_seq),
                        );
                    } else if applied {
                        if !create.pending_for.is_empty() {
                            let (parent, pending) = (shape.parent.clone(), shape.name.clone());
                            let (id, mark) = (&create.node_id, &create.pending_for);
                            trees.owe_finish(id, parent, pending, mark, result.seq);
                        }
                        agreed.extend(agree(trees, &create.node_id, result.seq));
                        synced.set(&create.node_id, Some(shape));
                    }
                    for (id, remote) in moved_in {
                        synced.set(&id, remote);
                    }
                    for id in trashed {
                        trees.hold_unsent_move(id);
                    }
                    let exists = taken && trees.remote().get(&create.node_id).is_some();
                    (
                        &create.node_id,
                        applied || exists || code == Code::MergedInto,
                    )
                }
                Some(op::Op::CreateFile(create)) => {
                    if applied {
                        let shape = Shape {
                            parent: parent(&create.parent_id),
                            name: name(&create.name),
                            kind: Kind::File,
                            content: create.content.clone(),
                        };
                        let shape = created(trees, &synced, &create.node_id, shape);
                        if trees.lost_position(&create.node_id).is_some() {
                            trees.take_lost_position(
                                create.node_id.clone(),
                                shape,
                                Origin::Commit(flight.commit.device_seq),
                            );
                        } else {
                            agreed.extend(agree(trees, &create.node_id, result.seq));
                            synced.set(&create.node_id, Some(shape));
                        }
                        trees.sent_edit(&create.node_id, create.content.as_ref());
                    }
                    let exists = taken && trees.remote().get(&create.node_id).is_some();
                    (&create.node_id, applied || exists)
                }
                Some(op::Op::UpdateContent(update)) => {
                    let id = &update.node_id;
                    let edited = synced.get(trees, id).map(|shape| Shape {
                        content: update.content.clone(),
                        ..shape
                    });
                    let current = trees.synced().live(id).and_then(|s| s.content.as_ref());
                    let downloaded =
                        code == Code::ConflictCopy && trees.local().get(&outcome.node_id).is_some();
                    if applied && trees.took_content(id) {
                        let kept = synced.get(trees, id).map(|shape| Shape {
                            content: current.cloned(),
                            ..shape
                        });
                        synced.set(id, kept);
                    } else if (applied && edited.is_some()) || downloaded {
                        synced.set(id, edited);
                    } else if code == Code::ConflictCopy {
                        synced.set(&outcome.node_id, edited);
                        synced.set(id, None);
                        rekey(trees, id, &outcome.node_id);
                        trees.sent_edit(&outcome.node_id, update.content.as_ref());
                    }
                    if applied && !update.resend {
                        trees.record_edited(id, update.content.as_ref());
                    }
                    if applied || code == Code::ConflictCopy {
                        trees.sent_edit(id, update.content.as_ref());
                    }
                    if code == Code::ConflictCopy {
                        conflicts.push((index, id.clone(), outcome.node_id.clone()));
                    }
                    (id, applied || code == Code::ConflictCopy)
                }
                Some(op::Op::Move(moved)) => {
                    let id = &moved.node_id;
                    let cycle = code == Code::Rejected && outcome.reject_reason == "CYCLE";
                    let target = parent(&moved.new_parent_id);
                    let before = synced.get(trees, id);
                    let finishing = (trees.pending_finishes().get(id))
                        .filter(|finish| finish.sending)
                        .map(|finish| finish.disk_first);
                    let superseded = code == Code::Superseded && moved.from.is_some();
                    let moved_back = finishing == Some(true)
                        && applied
                        && (trees.local().get(id).filter(|local| !local.parked)).is_some_and(
                            |local| {
                                let finish = &trees.pending_finishes()[id];
                                local.shape.parent == finish.parent
                                    && nfc(&local.shape.name) == nfc(&finish.pending)
                            },
                        );
                    if moved_back {
                        trees.restart_finish(id, result.seq);
                    } else if finishing.is_some() && (applied || superseded) {
                        trees.end_finish(id);
                    }
                    let finished = finishing == Some(false)
                        && (before.as_ref()).is_some_and(|before| {
                            (trees.local().get(id))
                                .is_some_and(|local| local.shape.same_position(before))
                        });
                    if let Some(shape) = before
                        && applied
                        && target != *id
                        && !finished
                    {
                        let shape = Shape {
                            parent: target.clone(),
                            name: name(&moved.new_name),
                            ..shape
                        };
                        if !moved.pending_for.is_empty() {
                            let (parent, pending) = (shape.parent.clone(), shape.name.clone());
                            trees.owe_finish(id, parent, pending, &moved.pending_for, result.seq);
                        }
                        agreed.extend(agree(trees, id, result.seq));
                        synced.set(id, Some(shape));
                    }
                    if applied || cycle || superseded {
                        trees.sent_move(id);
                    }
                    if cycle {
                        trees.revert(id.clone());
                    }
                    if superseded {
                        let local = (trees.local().get(id))
                            .filter(|local| !local.parked)
                            .map(|local| &local.shape)
                            .filter(|local| {
                                local.parent == target && nfc(&local.name) == nfc(&moved.new_name)
                            });
                        match (local, synced.get(trees, id)) {
                            (Some(local), Some(shape)) => synced.set(
                                id,
                                Some(Shape {
                                    parent: local.parent.clone(),
                                    name: local.name.clone(),
                                    ..shape
                                }),
                            ),
                            (Some(local), None) if trees.resends().contains(id) => {
                                synced.set(id, Some(local.clone()))
                            }
                            _ => {}
                        }
                        trees.end_resend(id);
                    }
                    (id, applied || cycle || superseded)
                }
                Some(op::Op::Delete(delete)) => {
                    let done = applied || code == Code::Superseded;
                    if done {
                        let kept = delete.requires.as_ref().map(|kept| kept.node_id.as_str());
                        for id in flight.deletes.get(&delete.node_id).into_iter().flatten() {
                            let revived = (trees.remote().get(id)).filter(|node| {
                                code == Code::Superseded
                                    && !node.trashed
                                    && node.meta_rev == result.seq
                                    && node.content_rev.max(node.edit_rev) <= flight.commit.base_seq
                                    && remote_below(trees, id, &revivers)
                            });
                            if let Some(revived) = revived {
                                synced.set(id, Some(revived.shape.clone()));
                                continue;
                            }
                            let retrashed = placed.contains(id) && trees.local().get(id).is_some();
                            if !retrashed {
                                synced.set(id, None);
                            }
                            let kept = kept.filter(|_| *id == delete.node_id);
                            trees.record_trashed(id, result.seq, kept);
                        }
                    }
                    (&delete.node_id, done)
                }
                _ => continue,
            };
            if applied && sent.is_some() {
                trees.record_moved(node, result.seq);
            }
            let created = matches!(&op.op, Some(op::Op::CreateDir(_) | op::Op::CreateFile(_)));
            if created
                && taken
                && let Some((sent_parent, sent_name)) = sent
                && let Some(remote) = trees.remote().get(node).filter(|remote| remote.trashed)
                && synced.get(trees, node).is_none()
            {
                let shape = Shape {
                    parent: merged.get(sent_parent).unwrap_or(sent_parent).clone(),
                    name: sent_name.clone(),
                    ..remote.shape.clone()
                };
                synced.set(node, Some(shape));
                let content = match &op.op {
                    Some(op::Op::CreateFile(create)) => create.content.as_ref(),
                    _ => None,
                };
                trees.sent_edit(node, content);
            }
            let published = created && done && self.published.remove(node);
            if renamed
                && !published
                && let Some((sent_parent, sent_name)) = sent
                && let Some(original) = holder(
                    trees,
                    merged.get(sent_parent).unwrap_or(sent_parent),
                    sent_name,
                    node,
                )
            {
                conflicts.push((index, original, node.clone()));
            }
            let follows = trees.local().get(node).is_some_and(|local| {
                let held = &local.shape.parent;
                aliases.contains_key(held) || merged.contains_key(held)
            });
            let left = sent.is_some_and(|(sent_parent, name)| {
                (trees.local().get(node)).is_none_or(|local| {
                    local.shape.parent != *merged.get(sent_parent).unwrap_or(sent_parent)
                        || nfc(&local.shape.name) != nfc(name)
                })
            });
            let kept = agreed.contains_key(node);
            if (renamed && !left && !kept) || (done && follows) {
                trees.revert(node.clone());
            } else if renamed && !left {
                unreverted.insert(node.clone());
            }
            if !done {
                self.problems.insert(node.clone());
            }
        }
        for (index, (op, outcome)) in flight.commit.ops.iter().zip(&result.results).enumerate() {
            let Some(op::Op::Restore(restore)) = &op.op else {
                continue;
            };
            let code = op_result::Code::try_from(outcome.code);
            if !matches!(
                code,
                Ok(op_result::Code::Applied | op_result::Code::AppliedRenamed)
            ) {
                continue;
            }
            let restored = BTreeSet::from([restore.node_id.as_str()]);
            let deleted = (flight.deletes.values())
                .filter(|planned| planned.contains(&restore.node_id))
                .flatten()
                .filter(|id| **id == restore.node_id || remote_below(trees, id, &restored));
            for id in flight
                .restores
                .get(&index)
                .into_iter()
                .flatten()
                .chain(deleted)
            {
                if trees.local().get(id).is_none() && trees.remote().live(id).is_some() {
                    synced.set(id, None);
                }
            }
        }
        let revived: Vec<NodeId> = (trees.pending_finishes().iter())
            .filter(|(id, finish)| finish.sending && !placed.contains(id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in revived {
            trees.restart_finish(&id, result.seq);
        }
        let mut rest = synced.apply(trees);
        let mut positions = Synced::default();
        for (id, at) in &agreed {
            let held = match rest.iter().find(|(other, _)| other == id) {
                Some((_, shape)) => shape.clone(),
                None => trees.synced().get(id).cloned(),
            };
            if let Some(shape) = held {
                let shape = Shape {
                    parent: at.parent.clone(),
                    name: at.name.clone(),
                    ..shape
                };
                positions.set(id, Some(shape));
            }
        }
        let mut taken: BTreeSet<NodeId> = positions.ids().collect();
        let (waits, dropped): (Vec<_>, Vec<_>) =
            (positions.apply(trees).into_iter()).partition(|(id, shape)| {
                let local = trees.local().get(id).map(|node| &node.shape);
                (local.into_iter().chain(trees.remote().live(id))).any(|held| {
                    shape
                        .as_ref()
                        .is_some_and(|agreed| held.same_position(agreed))
                })
            });
        for (id, _) in dropped {
            taken.remove(&id);
        }
        rest.retain(|(id, _)| !taken.contains(id));
        for id in unreverted.iter().filter(|id| !taken.contains(*id)) {
            trees.revert(id.clone());
        }
        for (id, shape) in rest.into_iter().chain(waits) {
            trees.wait(id, shape, Origin::Commit(flight.commit.device_seq));
        }
        let mut closed = BTreeSet::new();
        for (id, index, sent, code, made) in answers {
            use op_result::Code;
            let question = self
                .questions
                .get_mut()
                .iter_mut()
                .find(|question| question.id == id);
            let Some(question) = question else {
                continue;
            };
            let QuestionKind::Conflict {
                original, sent_in, ..
            } = &mut question.kind
            else {
                continue;
            };
            *sent_in = None;
            match (sent, code) {
                (op::Op::Delete(delete), Code::Applied) => {
                    let kept = delete.requires.as_ref().map(|kept| kept.node_id.as_str());
                    trees.record_trashed(&delete.node_id, result.seq, kept);
                    closed.insert(id);
                }
                (op::Op::UpdateContent(update), Code::Applied) => {
                    trees.record_edited(&update.node_id, update.content.as_ref());
                    let remote = trees.remote().get(&original.node_id);
                    let took = remote.filter(|node| {
                        (node.shape.content.as_ref().zip(update.content.as_ref()))
                            .is_some_and(|(now, sent)| same_content(now, sent))
                    });
                    if let Some(node) = took {
                        original.revisions.content_rev = node.content_rev;
                    }
                }
                (op::Op::UpdateContent(_), Code::ConflictCopy) => {
                    question.answer = None;
                    conflicts.push((index, original.node_id.clone(), made.clone()));
                }
                _ => question.answer = None,
            }
        }
        (self.questions.get_mut()).retain(|question| !closed.contains(&question.id));
        for (index, original, copy) in conflicts {
            self.ask_conflict(trees, &flight.commit.commit_id, index, original, copy);
        }
        let restored = carried(&self.restores, &flight.commit.ops);
        for (index, id) in restored.expect("a Restore op in flight is queued") {
            self.resolve(id, Outcome::from_result(&result.results[index]));
        }
        trees.watch_content([]);
        trees.watch_positions([]);
        self.retry_waiting(trees, BTreeSet::new(), Vec::new());
        self.keep_unsent(trees, sent);
        self.accepted.set(flight.commit.device_seq);
        self.slots.insert(flight.commit.device_seq, result.seq);
        while self.slots.len() > SLOTS_KEPT {
            self.slots.pop_first();
        }
        if self.slots.len() == SLOTS_KEPT
            && let Some((_, &oldest)) = self.slots.first_key_value()
        {
            trees.forget_past_positions_below(oldest);
        }
        if cfg!(debug_assertions) {
            let away: Vec<&NodeId> = (lost.iter())
                .map(|id| match trees.local().get(id) {
                    Some(_) => id,
                    None => merged.get(id).unwrap_or(id),
                })
                .filter(|id| {
                    let synced = match trees.waiting().get(*id) {
                        Some(waiting) => waiting.shape.as_ref(),
                        None => trees.synced().live(id),
                    };
                    (trees.local().live(id).zip(synced)).is_some_and(|(l, s)| !l.same_position(s))
                })
                .collect();
            assert!(
                away.is_empty(),
                "acknowledging creates sent at a lost position left S away from L's position: {away:?}"
            );
        }
        self.cap
            .set(self.cap.map(|cap| cap * 2).filter(|cap| *cap < MAX_OPS));
        true
    }

    /// Makes the waiting S updates that fit now, together with S := ⊥ for the `needed` nodes the
    /// keep rule could not clear yet and the `agreed` updates of the rebase, which may depend on
    /// each other. Returns the needed nodes still not cleared; agreed updates that do not fit are
    /// dropped, since the next rebase finds them again, except that one for a node whose update
    /// waited waits in its place, since the rebase took it from the waiting shape. A needed node whose own update waited
    /// keeps that update waiting, so it is made once it fits, also after the keep rule no longer
    /// needs the node. Each update that waits again keeps its origin.
    fn retry_waiting(
        &mut self,
        trees: &mut Trees,
        needed: BTreeSet<NodeId>,
        agreed: Vec<(NodeId, Option<Shape>)>,
    ) -> BTreeSet<NodeId> {
        let mut waited: BTreeMap<NodeId, Waiting> = (trees.take_waiting().into_iter())
            .filter_map(|(id, Waiting { shape, origin })| {
                let shape = outstanding(trees, &id, shape)?;
                Some((id, Waiting { shape, origin }))
            })
            .collect();
        let updates = (waited.iter())
            .map(|(id, waiting)| (id.clone(), waiting.shape.clone()))
            .chain(needed.iter().map(|id| (id.clone(), None)))
            .chain(agreed.iter().cloned())
            .collect();
        let mut still = BTreeSet::new();
        for (id, shape) in update_synced(trees, updates) {
            if needed.contains(&id) {
                if let Some(waiting) = waited.remove(&id) {
                    trees.wait(id.clone(), waiting.shape, waiting.origin);
                }
                still.insert(id);
            } else if let Some(waiting) = waited.get(&id) {
                trees.wait(id, shape, waiting.origin);
            } else if !agreed.iter().any(|(other, _)| *other == id) {
                trees.wait(id, shape, Origin::Other);
            }
        }
        still
    }

    /// What a call-level error on the commit in flight means. An error that proves nothing was
    /// applied forgets the commit; one that leaves the outcome unknown keeps it to resend; a
    /// history mismatch or a stale device state keeps it as evidence and stops committing; and
    /// every other code, known or not, is a policy halt ([`Halt::Policy`], RFC 0006 section 1).
    /// `QUOTA_EXCEEDED` is resent only when it names a retry time, after that wait.
    ///
    /// The restores of a forgotten commit stay queued and go into the next commit, except on a
    /// device that is now read-only, where they end as rejected. A halt keeps them queued.
    pub fn failed(
        &mut self,
        trees: &mut Trees,
        error: &proto::Error,
    ) -> Result<Failure, CommitError> {
        use error::Code;
        let Some(flight) = &*self.in_flight else {
            return Err(CommitError::NotInFlight);
        };
        let head_moved = (error.code == Code::HeadMoved as i32).then_some(flight.commit.base_seq);
        let failure = match Code::try_from(error.code) {
            Ok(Code::HistoryMismatch) => Failure::Halted(Halt::Recovery(RecoveryCause::of(
                RecoveryTrigger::SpaceRewound,
            ))),
            Ok(Code::StaleDeviceState) => Failure::Halted(Halt::Recovery(RecoveryCause::of(
                RecoveryTrigger::StoreRolledBack,
            ))),
            Ok(Code::ReadOnly) => Failure::Halted(Halt::ReadOnly),
            Ok(Code::AheadOfServer) => Failure::Resync,
            Ok(Code::LimitExceeded) => Failure::Split,
            Ok(Code::HeadMoved | Code::UnsupportedVersion) => Failure::Abandoned,
            Ok(Code::QuotaExceeded) if error.retry_after_ms > 0 => Failure::Resend,
            Ok(Code::MissingChunks | Code::RetryLater | Code::Unauthenticated | Code::Internal) => {
                Failure::Resend
            }
            _ => Failure::Halted(Halt::Policy { code: error.code }),
        };
        match failure {
            Failure::Halted(halt @ (Halt::Recovery(_) | Halt::Policy { .. })) => {
                self.halted.set(Some(halt))
            }
            Failure::Resend => {}
            Failure::Halted(halt) => {
                self.halted.set(Some(halt));
                self.in_flight.set(None);
            }
            Failure::Abandoned | Failure::Resync => self.in_flight.set(None),
            Failure::Split => {
                let flight = self.in_flight.get_mut().take().expect("checked");
                let ops = flight.commit.ops;
                let restored = carried(&self.restores, &ops);
                let restore = restored
                    .expect("a Restore op in flight is queued")
                    .remove(&0);
                if ops.len() > 1 {
                    self.cap.set(Some(ops.len() / 2));
                } else if let Some(id) = restore {
                    self.resolve(id, Outcome::DoesNotFit);
                } else if let Some(op) = ops.into_iter().next().and_then(|op| op.op) {
                    match op {
                        op::Op::Delete(delete) if !self.split.contains(&delete.node_id) => {
                            self.split.insert(delete.node_id);
                        }
                        op => {
                            self.problems.insert(op_node(&op).to_owned());
                        }
                    }
                }
            }
        }
        if self.in_flight.is_none() {
            trees.unsend_finishes(head_moved);
        }
        self.reject_unsent_when_read_only();
        Ok(failure)
    }

    /// Forgets the commit in flight when the driver knows it was not applied: after
    /// `MISSING_CHUNKS` when the file changed since it was chunked (section 4.2). False when no
    /// commit is in flight, its result has arrived, or a policy halt keeps it.
    pub fn abandon(&mut self) -> bool {
        if matches!(*self.halted, Some(Halt::Policy { .. })) {
            return false;
        }
        self.in_flight
            .take_if(|flight| flight.result.is_none())
            .is_some()
    }

    /// Forgets the commit in flight, which the server holds nothing of, because the content of
    /// `node` could not be read for its upload, and leaves `node` out of planning until
    /// [`Commits::clear_problem`], so that the next commit carries every other change. False, and
    /// nothing changes, when [`Commits::abandon`] would refuse.
    pub fn leave_out(&mut self, node: &str) -> bool {
        if !self.abandon() {
            return false;
        }
        self.problems.insert(node.to_owned());
        true
    }

    /// Compares the `Welcome` with the commit in flight and the accepted counter *a* (section
    /// 2.8). With *w* the server's counter, *w* = *a* means the commit in flight was not accepted
    /// and *w* = *a* + 1 that it was and its response was lost; both resend it unchanged, and
    /// the server answers an accepted one with its stored result. *w* < *a* means the server lost
    /// commits it had accepted, since its counter only grows and *a* takes a slot only from a
    /// result. So does *w* = *a* while the result of the commit in flight is held, since the
    /// server sent that result. Either way *a* takes *w*, and the commit in flight is dropped, its
    /// restores staying queued, so that its changes are planned again once R is listed again. Any other *w*, or a commit
    /// built in another history, starts recovery. A read-only device resends nothing. A device
    /// under a policy halt resends nothing whatever the `Welcome` says, but *w* < *a* is a loss
    /// there too: *a* takes *w*, the refused commit is dropped, since its slot is no longer the one
    /// after the server's count, and R is listed again, while the halt stays. The slots above *w*
    /// stay until R is listed again ([`Commits::forget_lost_slots`]), so that
    /// [`Commits::lost_here`] still names the commits the server lost, while
    /// [`Commits::held_through`] counts none above *a*. A later `Welcome` finds *w* = *a*, so a
    /// loss is handled once. Whether R
    /// itself belongs to the `Welcome`'s history is checked where R is kept.
    ///
    /// Recovery keeps the queued restores with the commit in flight: nothing is built under that
    /// halt, so none is sent again or planned again, and each stays as the request it was
    /// (section 6.4). On a read-only device the restores no commit in flight carries end as
    /// rejected.
    pub fn connected(&mut self, welcome: &proto::Welcome) -> Reconnect {
        let (a, w) = (*self.accepted, welcome.device_seq);
        let policy = matches!(*self.halted, Some(Halt::Policy { .. }));
        let reconnect = match &*self.in_flight {
            _ if policy && w >= a => return Reconnect::Idle,
            _ if policy => Reconnect::AheadOfServer,
            _ if matches!(*self.halted, Some(Halt::Recovery(_))) => Reconnect::Recovery,
            Some(flight) if flight.commit.history_id != welcome.history_id => Reconnect::Recovery,
            None if w == a => Reconnect::Idle,
            Some(flight) if flight.result.is_none() && (w == a || w == a + 1) => Reconnect::Resend,
            Some(_) if w == a + 1 => Reconnect::Idle,
            Some(_) if w == a => Reconnect::AheadOfServer,
            _ if w < a => Reconnect::AheadOfServer,
            _ => Reconnect::Recovery,
        };
        if reconnect == Reconnect::Recovery {
            let rewound = (self.in_flight.as_ref())
                .is_some_and(|flight| flight.commit.history_id != welcome.history_id);
            let cause = match *self.halted {
                Some(Halt::Recovery(cause))
                    if !(rewound
                        && cause.trigger == RecoveryTrigger::SpaceRewound
                        && cause.restore_seq.is_none()) =>
                {
                    cause
                }
                _ if rewound => {
                    RecoveryCause::rewind(welcome.restore_seq, welcome.restore_point_ms)
                }
                _ => RecoveryCause::of(RecoveryTrigger::StoreRolledBack),
            };
            self.halted.set(Some(Halt::Recovery(cause)));
            return reconnect;
        }
        if reconnect == Reconnect::AheadOfServer {
            self.accepted.set(w);
            self.in_flight.set(None);
        }
        if policy {
            return reconnect;
        }
        if welcome.read_only {
            self.halted.set(Some(Halt::ReadOnly));
            self.reject_unsent_when_read_only();
            return match reconnect {
                Reconnect::Resend => Reconnect::Idle,
                other => other,
            };
        }
        if *self.halted == Some(Halt::ReadOnly) {
            self.halted.set(None);
        }
        reconnect
    }
}

/// The queued restore each `Restore` op of a commit carries, by the op's index: the first in
/// request order whose op equals it and that no earlier op took. Queued restores with equal ops
/// are interchangeable, so it makes no difference which of them an op is counted for. Err with
/// the index of a `Restore` op that no queued restore is left for.
fn carried(
    restores: &BTreeMap<RestoreId, RestoreIntent>,
    ops: &[proto::Op],
) -> Result<BTreeMap<usize, RestoreId>, usize> {
    let mut queued: HashMap<&proto::Restore, VecDeque<&RestoreId>> = HashMap::new();
    for (id, intent) in restores {
        queued.entry(intent.op()).or_default().push_back(id);
    }
    let mut found = BTreeMap::new();
    for (index, op) in ops.iter().enumerate() {
        if let Some(op::Op::Restore(restore)) = &op.op {
            let id = queued.get_mut(restore).and_then(VecDeque::pop_front);
            found.insert(index, id.ok_or(index)?.clone());
        }
    }
    Ok(found)
}

fn positioned(op: &op::Op) -> Option<&str> {
    match op {
        op::Op::CreateDir(create) => Some(&create.node_id),
        op::Op::CreateFile(create) => Some(&create.node_id),
        op::Op::Move(moved) => Some(&moved.node_id),
        _ => None,
    }
}

fn op_node(op: &op::Op) -> &str {
    match op {
        op::Op::CreateDir(op) => &op.node_id,
        op::Op::CreateFile(op) => &op.node_id,
        op::Op::UpdateContent(op) => &op.node_id,
        op::Op::Move(op) => &op.node_id,
        op::Op::Delete(op) => &op.node_id,
        op::Op::Restore(op) => &op.node_id,
    }
}

/// The S updates an acknowledgement makes, in op order, made together at the end.
#[derive(Default)]
struct Synced {
    updates: Vec<(NodeId, Option<Shape>)>,
}

impl Synced {
    fn set(&mut self, id: &str, shape: Option<Shape>) {
        self.updates.push((id.to_owned(), shape));
    }

    /// S for `id` with the updates so far. A waiting update stands in for S.
    fn get(&self, trees: &Trees, id: &str) -> Option<Shape> {
        match self.updates.iter().rev().find(|(other, _)| other == id) {
            Some((_, shape)) => shape.clone(),
            None => trees.acknowledged(id).cloned(),
        }
    }

    /// Makes the updates that leave S valid and returns the rest (see [`update_synced`]).
    fn ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.updates.iter().map(|(id, _)| id.clone())
    }

    fn apply(self, trees: &mut Trees) -> Vec<(NodeId, Option<Shape>)> {
        let taken: BTreeMap<NodeId, Option<Shape>> = self.updates.iter().cloned().collect();
        let left = update_synced(trees, self.updates);
        for (id, shape) in taken {
            if let Some(shape) = shape {
                trees.sent_shape(&id, &shape);
            }
        }
        left
    }
}

/// Whether planning deletes the node: L deleted it, S holds it, and the keep rule does not need it.
fn deletes(trees: &Trees, id: &str, needed: &BTreeSet<NodeId>) -> bool {
    trees.local().get(id).is_none()
        && trees.synced().live(id).is_some()
        && trees.acknowledged_class(id) == Class::Local
        && !needed.contains(id)
}

/// Makes the S updates, the last for each node, that leave S a valid tree: all of them together
/// when they fit together, otherwise those left once each update that a node outside the batch
/// blocks is taken out ([`blocked`]), otherwise each that fits on its own and then the rest together
/// again, while any is made. Returns the rest.
fn update_synced(
    trees: &mut Trees,
    updates: Vec<(NodeId, Option<Shape>)>,
) -> Vec<(NodeId, Option<Shape>)> {
    let mut left: Vec<(NodeId, Option<Shape>)> = Vec::new();
    for (id, shape) in updates {
        left.retain(|(other, _)| *other != id);
        left.push((id, shape));
    }
    while !left.is_empty() {
        if set_synced_together(trees, &left) {
            return Vec::new();
        }
        let mut fitting = left.clone();
        let mut out = Vec::new();
        loop {
            let batch: BTreeMap<NodeId, Option<Shape>> = fitting.iter().cloned().collect();
            let (stuck, rest): (Vec<_>, Vec<_>) = (fitting.into_iter())
                .partition(|(id, shape)| blocked(trees, &batch, id, shape.as_ref()));
            fitting = rest;
            if stuck.is_empty() {
                break;
            }
            out.extend(stuck);
        }
        if !out.is_empty() && !fitting.is_empty() && set_synced_together(trees, &fitting) {
            left = out;
            continue;
        }
        let count = left.len();
        left.retain(|(id, shape)| !trees.set_synced_if_valid(id.clone(), shape.clone()));
        if left.len() == count {
            break;
        }
    }
    left
}

/// Sets S for every node of `updates` when S stays a valid tree with all of them. Returns whether it
/// did.
fn set_synced_together(trees: &mut Trees, updates: &[(NodeId, Option<Shape>)]) -> bool {
    if !trees.fits_synced_together(updates) {
        return false;
    }
    for (id, shape) in updates {
        trees.set_synced(id.clone(), shape.clone());
    }
    true
}

/// Whether a node outside `batch` keeps the update of `id` to `shape` from fitting, whatever else
/// the batch makes: S gives the name it takes to a node the batch does not update, or neither S nor
/// the batch holds its folder, or S holds a node the batch does not update under the node it
/// removes.
fn blocked(
    trees: &Trees,
    batch: &BTreeMap<NodeId, Option<Shape>>,
    id: &str,
    shape: Option<&Shape>,
) -> bool {
    let Some(shape) = shape else {
        return trees
            .synced_children(id)
            .any(|child| !batch.contains_key(child));
    };
    let placed = (batch.get(&shape.parent))
        .is_some_and(|next| next.as_ref().is_some_and(|next| next.kind == Kind::Dir));
    let held = !batch.contains_key(&shape.parent)
        && (trees.synced().live(&shape.parent)).is_some_and(|parent| parent.kind == Kind::Dir);
    let root = shape.parent == trees.synced().root;
    let taken = (trees.synced_holders(&shape.parent, &shape.name))
        .any(|child| child != id && !batch.contains_key(child));
    !(root || placed || held) || taken
}

/// What changed about the files of a conflict since it was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drift {
    None,
    /// R holds another content revision of a file.
    Content,
    Ended,
}

fn drift(trees: &Trees, original: &ConflictFile, copy: &ConflictFile) -> Drift {
    let files = [original, copy];
    let moved_here = |id: &str| {
        (trees.acknowledged(id)).is_some_and(|held| {
            (trees.local().get(id)).is_none_or(|local| {
                !trees.reverted().contains(id) && !local.shape.same_position(held)
            })
        })
    };
    let ended = files.iter().any(|file| {
        let remote = trees
            .remote()
            .get(&file.node_id)
            .filter(|node| !node.trashed);
        moved_here(&file.node_id)
            || remote.is_none_or(|node| node.meta_rev != file.revisions.meta_rev)
    });
    let changed = files.iter().any(|file| {
        let remote = trees.remote().get(&file.node_id);
        remote.is_some_and(|node| node.content_rev != file.revisions.content_rev)
    });
    match (ended, changed) {
        (true, _) => Drift::Ended,
        (false, true) => Drift::Content,
        (false, false) => Drift::None,
    }
}

/// Whether L, S and R agree on both files of a conflict.
fn in_sync(trees: &Trees, original: &ConflictFile, copy: &ConflictFile) -> bool {
    [original, copy]
        .iter()
        .all(|file| trees.class(&file.node_id) == Class::InSync)
}

fn leaving_unsent(
    scratch: &Scratch,
    aliases: &BTreeMap<NodeId, NodeId>,
    problems: &BTreeSet<NodeId>,
    id: &str,
) -> bool {
    let trees = scratch.trees;
    let (Some(local), Some(parent), Some(name)) =
        (trees.local().get(id), scratch.parent(id), scratch.name(id))
    else {
        return false;
    };
    (problems.contains(id) || trees.waiting().contains_key(id) || trees.excluded(id))
        && (aliases
            .get(&local.shape.parent)
            .unwrap_or(&local.shape.parent)
            != parent
            || name_key(&local.shape.name) != name_key(name))
}

/// The node other than `other` that R holds live under `parent` with the name key of `name`.
fn holder(trees: &Trees, parent: &str, name: &str, other: &str) -> Option<NodeId> {
    let key = name_key(name);
    (trees.remote().iter())
        .find(|(id, node)| {
            !node.trashed
                && *id != other
                && node.shape.parent == parent
                && name_key(&node.shape.name) == key
        })
        .map(|(id, _)| id.clone())
}

/// The entries `result` gives a node, each as the ID `commit` sent for it and the node's ID: a
/// create the server applied, a folder create it merged into another folder, and a content update
/// it kept as a conflict copy.
pub fn given_nodes(commit: &proto::Commit, result: &proto::CommitResult) -> Vec<(NodeId, NodeId)> {
    use op_result::Code;
    (commit.ops.iter().zip(&result.results))
        .filter_map(|(sent, outcome)| {
            let code = Code::try_from(outcome.code).unwrap_or(Code::Unspecified);
            let (id, given) = match (sent.op.as_ref()?, code) {
                (op::Op::CreateDir(create), Code::MergedInto) => {
                    (&create.node_id, &outcome.node_id)
                }
                (op::Op::CreateDir(create), Code::Applied | Code::AppliedRenamed) => {
                    (&create.node_id, &create.node_id)
                }
                (op::Op::CreateFile(create), Code::Applied | Code::AppliedRenamed) => {
                    (&create.node_id, &create.node_id)
                }
                (op::Op::UpdateContent(update), Code::ConflictCopy) => {
                    (&update.node_id, &outcome.node_id)
                }
                _ => return None,
            };
            Some((id.clone(), given.clone()))
        })
        .collect()
}

/// Gives the local node `from` the ID `to`; its children follow, and so does a folder's unknown
/// rules ([`Trees::set_rules_unknown`]).
pub(crate) fn rekey(trees: &mut Trees, from: &str, to: &str) {
    let Some(node) = trees.local().get(from).cloned() else {
        return;
    };
    let rules_unknown = trees.unknown_rules().contains(from);
    let children: Vec<NodeId> = (trees.local_children_placed_or_parked(from))
        .cloned()
        .collect();
    let parked_name = trees.parked_name(from).to_owned();
    let parked = node.parked;
    trees.rekey_discarded(from, to);
    trees.remove_local(from);
    trees.set_local_changed(to.to_owned(), node);
    if parked {
        trees.park_as(to.to_owned(), parked_name);
    }
    if rules_unknown {
        trees.set_rules_unknown(to);
    }
    for id in children {
        trees.rekey_parent(id, to);
    }
    let waiting: Vec<(NodeId, Shape, Origin)> = (trees.waiting().iter())
        .filter_map(|(id, waiting)| {
            let shape = waiting
                .shape
                .as_ref()
                .filter(|shape| shape.parent == from)?;
            let shape = Shape {
                parent: to.to_owned(),
                ..shape.clone()
            };
            Some((id.clone(), shape, waiting.origin))
        })
        .collect();
    for (id, shape, origin) in waiting {
        trees.wait(id, Some(shape), origin);
    }
}

/// The rebase of section 2.4, before ops are planned. A reverted file that R no longer has but that
/// holds an edit not uploaded yet becomes a new node, so the edit is uploaded as a create instead
/// of being removed with the node. Nodes in `skip` are not rebased otherwise, though what they changed
/// remotely still keeps their ancestors. An excluded node is not rebased and keeps nothing; S
/// still takes what both sides agree on for it. Returns the nodes the keep rule could
/// not clear from S yet, and the S updates both sides agree on, which are made together with the
/// waiting ones.
fn rebase(
    trees: &mut Trees,
    ids: &mut IdGen,
    now_ms: u64,
    skip: &BTreeSet<NodeId>,
) -> (BTreeSet<NodeId>, Vec<(NodeId, Option<Shape>)>) {
    let dirty: Vec<NodeId> = trees.dirty_others().iter().cloned().collect();
    for id in &dirty {
        let held = trees.reverted().contains(id)
            || trees.waiting().contains_key(id)
            || (trees.local().get(id)).is_some_and(|node| node.parked)
                && (trees.remote().get(id).is_some() || trees.synced().get(id).is_some());
        if held
            && trees.remote().live(id).is_none()
            && trees.edited(id)
            && !trees.excluded(id)
            && !trees.held(id)
        {
            rekey(trees, id, &ids.uuid_v7(now_ms));
        }
    }
    for id in dirty.iter().filter(|id| !skip.contains(*id)) {
        if trees.acknowledged_class(id) != Class::Concurrent
            || !edited_on_both_sides(trees, id)
            || trees.excluded(id)
        {
            continue;
        }
        if trees.take_forgotten_shape(id) {
            continue;
        }
        if trees.write_held_beside(id) {
            trees.take_local(id.clone());
            continue;
        }
        rekey(trees, id, &ids.uuid_v7(now_ms));
    }
    for id in &dirty {
        let (Some(local), Some(remote)) = (trees.local().get(id), trees.remote().live(id)) else {
            continue;
        };
        if !local.parked
            || trees.acknowledged_class(id) != Class::Remote
            || trees.excluded(id)
            || !effects::held_by_excluded(trees, id, remote)
        {
            continue;
        }
        let mut node = local.clone();
        node.shape.parent = remote.parent.clone();
        node.shape.name = remote.name.clone();
        trees.set_local(id.clone(), node);
    }
    let mut needed = BTreeSet::new();
    let mut agreed = Vec::new();
    for id in &dirty {
        keep(trees, id, &mut needed);
        if skip.contains(id) {
            continue;
        }
        match trees.acknowledged_class(id) {
            Class::Concurrent => {
                agreed.extend(take_agreed(trees, id).map(|s| (id.clone(), Some(s))))
            }
            Class::Same => agreed.push((id.clone(), trees.remote().live(id).cloned())),
            _ => {}
        }
    }
    (needed, agreed)
}

/// S takes the part of a concurrent change that both sides made the same way, position or
/// content, so only the parts that differ are left to rebase. None when they agree on nothing new.
fn take_agreed(trees: &Trees, id: &str) -> Option<Shape> {
    let (Some(local), Some(synced), Some(remote)) = (
        trees.local().get(id).map(|node| &node.shape),
        trees.acknowledged(id),
        trees.remote().live(id),
    ) else {
        return None;
    };
    let mut next = synced.clone();
    if local.same_position(remote) {
        next.parent = remote.parent.clone();
        next.name = remote.name.clone();
    }
    if let (Some(l), Some(r)) = (&local.content, &remote.content)
        && same_content(l, r)
    {
        next.content = Some(r.clone());
    }
    (!next.same(synced)).then_some(next)
}

/// A file whose content changed on both sides, each differently (R3). A file of
/// [`Trees::unsent_edits`] changed locally even where L holds S's bytes.
fn edited_on_both_sides(trees: &Trees, id: &str) -> bool {
    let content = |shape: Option<&Shape>| shape.and_then(|shape| shape.content.clone());
    let (Some(l), Some(r)) = (
        content(trees.local().live(id)),
        content(trees.remote().live(id)),
    ) else {
        return false;
    };
    let s = content(trees.acknowledged(id));
    let changed = |c: &Content| s.as_ref().is_none_or(|s| !same_content(c, s));
    (changed(&l) || trees.unsent_edits().contains(id)) && changed(&r) && !same_content(&l, &r)
}

fn holds_server_entry(trees: &Trees, id: &str) -> bool {
    trees.local_children(id).any(|child| {
        !trees.excluded(child)
            && (trees.remote().get(child).is_some()
                || trees.synced().get(child).is_some()
                || holds_server_entry(trees, child))
    })
}

fn parked_until_moved(trees: &Trees) -> impl Iterator<Item = &NodeId> {
    let known = |id: &str| trees.remote().get(id).is_some() || trees.synced().get(id).is_some();
    (trees.dirty_others().iter())
        .filter_map(|id| Some((id, trees.local().get(id)?)))
        .filter(move |(id, node)| {
            node.parked
                && !effects::kept_out(trees, id)
                && (known(id) || effects::unparks_to(trees, id, &node.shape))
        })
        .map(|(id, _)| id)
}

fn keep(trees: &mut Trees, id: &str, needed: &mut BTreeSet<NodeId>) {
    let deleted: Vec<NodeId> = (trees.keep_walk(id))
        .filter(|(_, deleted)| *deleted)
        .map(|(node, _)| node.to_owned())
        .collect();
    for node in deleted {
        if !trees.set_synced_if_valid(node.clone(), None) {
            trees.revert(node.clone());
            needed.insert(node);
        }
    }
}

/// The shape a create sent, with the content S holds when it changed since the create was built
/// ([`Trees::took_content`]).
fn created(trees: &Trees, synced: &Synced, id: &str, sent: Shape) -> Shape {
    match synced.get(trees, id) {
        Some(held) if trees.took_content(id) => Shape {
            content: held.content,
            ..sent
        },
        _ => sent,
    }
}

/// The position of `id` that L and R agreed on after `seq` ([`Trees::position_after`]), if any.
fn agree(trees: &Trees, id: &str, seq: u64) -> Option<(NodeId, Agreed)> {
    (trees.position_after(id, seq)).map(|at| (id.to_owned(), at.clone()))
}

pub(crate) fn disk_first(trees: &Trees, id: &str) -> bool {
    (trees.pending_finishes().get(id)).is_some_and(|finish| finish.disk_first)
}

fn wedged(
    trees: &Trees,
    wants: &[Option<Want>],
    scratch: &Scratch,
    moving: &BTreeSet<NodeId>,
    problems: &BTreeSet<NodeId>,
) -> Vec<(NodeId, NodeId)> {
    let targets: BTreeMap<&NodeId, (&NodeId, Fit)> = (wants.iter().flatten())
        .filter_map(|want| match want {
            Want::Create { id, shape, .. } => {
                Some((id, (&shape.parent, scratch.fit(want, moving))))
            }
            Want::Move { id, parent, .. } => Some((id, (parent, scratch.fit(want, moving)))),
            _ => None,
        })
        .collect();
    let under_reverted_merge = |id: &NodeId| {
        (trees.local().get(id)).is_some_and(|local| {
            local.shape.parent != *targets[id].0 && trees.reverted().contains(&local.shape.parent)
        })
    };
    let end = |id: &NodeId| {
        let mut seen = BTreeSet::from([id]);
        let mut at = targets[id].0;
        if under_reverted_merge(id) {
            return None;
        }
        loop {
            let pending = matches!(trees.class(at), Class::Remote | Class::Concurrent);
            let create = trees.dirty_creates().contains(at);
            if problems.contains(at)
                || trees.is_discarded(at)
                || (pending && !trees.excluded(at))
                || (create && (trees.excluded(at) || trees.held(at)))
            {
                return None;
            }
            if !seen.insert(at) {
                return Some(at.clone());
            }
            match targets.get(at) {
                Some((_, fit)) if *fit != Fit::Wait => return None,
                Some(_) if under_reverted_merge(at) => return None,
                Some((parent, _)) => at = parent,
                None if create => {
                    at = &trees
                        .local()
                        .get(at)
                        .expect("creates are in L")
                        .shape
                        .parent
                }
                None => return Some(at.clone()),
            }
        }
    };
    (targets.iter())
        .filter(|(_, (_, fit))| *fit == Fit::Wait)
        .filter_map(|(id, _)| Some(((*id).clone(), end(id)?)))
        .collect()
}

pub(crate) fn finishes<'a>(
    trees: &'a Trees,
    skip: &'a BTreeSet<NodeId>,
    device_id: Option<&'a str>,
) -> impl Iterator<Item = (&'a NodeId, &'a NodeId, &'a str, bool)> + 'a {
    let recorded = (trees.pending_finishes().keys()).filter(|id| !trees.marked().contains(*id));
    (trees.marked().iter())
        .chain(recorded)
        .filter(|id| {
            !skip.contains(*id)
                && !trees.excluded(id)
                && !trees.held(id)
                && !trees.waiting().contains_key(*id)
        })
        .filter_map(move |id| {
            let remote = trees.remote().get(id).filter(|node| !node.trashed)?;
            let finish = trees.pending_finishes().get(id);
            let name = match finish {
                Some(finish) if finish.sending || finish.base.is_some() => return None,
                Some(finish)
                    if finish.parent == remote.shape.parent
                        && finish.pending == remote.shape.name =>
                {
                    finish.name.as_str()
                }
                Some(_) => return None,
                None => remote.pending_for.as_deref()?,
            };
            let local = trees.local().get(id).filter(|node| !node.parked)?;
            let synced = trees.synced().live(id)?;
            let parent = &remote.shape.parent;
            let free = holder(trees, parent, name, id).is_none()
                && trees.local_holders(parent, name).all(|other| other == id);
            let owed = finish.is_some_and(|finish| {
                finish.origin == FinishOrigin::Legacy || remote.pending_for.is_none()
            }) || (remote.pending_by.is_some()
                && remote.pending_by.as_deref() == device_id);
            if !local.shape.same_position(&remote.shape)
                || !synced.same_position(&remote.shape)
                || !(free || owed)
            {
                return None;
            }
            let (mut names, kind) = path(trees.local(), id)?;
            *names.last_mut()? = name;
            (!trees.ignores(&names, kind)).then_some((id, parent, name, free))
        })
}

pub(crate) fn changed_locally(trees: &Trees, id: &str) -> bool {
    let Some(Waiting {
        shape: Some(waiting),
        ..
    }) = trees.waiting().get(id)
    else {
        return false;
    };
    let remote = trees.remote().live(id);
    let Some(local) = trees.local().live(id) else {
        return remote.is_some_and(|remote| remote.same_position(waiting));
    };
    let edited = local.kind != waiting.kind
        || match (&local.content, &waiting.content) {
            (Some(a), Some(b)) => !same_content(a, b),
            (a, b) => a.is_some() != b.is_some(),
        };
    let moved = !local.same_position(waiting)
        && (trees.remote().get(id)).is_some_and(|remote| remote.shape.same_position(waiting));
    edited || moved
}

/// What is left of a waiting S update. Its position is stale once R no longer has the node there
/// and L has left it too: it then falls back to the position S holds, or to R's when S does not
/// hold the node, so L's and R's positions are compared with the S they came from, as for any
/// change both sides made. Its content stays, so the acknowledged content remains the base L's
/// content is compared with. None when nothing is left: the update comes to what S holds, or
/// neither L nor R holds the node. A removal of a node S still holds stays, even then and while R
/// holds the node, only as long as what S holds under it leaves S with L's next commits
/// ([`leaves_s`]), so that R taking the node back meanwhile does not read as L removing it. Once
/// it cannot leave, the removal could never be made, and the keep rule brings the node back.
fn outstanding(trees: &Trees, id: &str, waiting: Option<Shape>) -> Option<Option<Shape>> {
    let local = trees.local().get(id).map(|node| &node.shape);
    let synced = trees.synced().live(id);
    let Some(remote) = trees.remote().live(id) else {
        let removal = waiting.is_none() && synced.is_some() && leaves_s(trees, id);
        return (local.is_some() || removal).then_some(waiting);
    };
    let Some(mut left) = waiting else {
        return leaves_s(trees, id).then_some(None);
    };
    if !left.same_position(remote) && !local.is_some_and(|local| local.same_position(&left)) {
        let base = synced.unwrap_or(remote);
        left.parent = base.parent.clone();
        left.name = base.name.clone();
    }
    (synced != Some(&left)).then_some(Some(left))
}

/// Whether everything S holds under `id` leaves S with L's commits: each node L holds there is
/// elsewhere in L, and each one L does not hold leaves with what S holds under it in turn. A node
/// L still holds where S has it, such as one parked under a folder the disk removed, keeps `id`
/// in S, and so does a reverted node R holds there, since no commit sends its local position and
/// the effects move it back.
fn leaves_s(trees: &Trees, id: &str) -> bool {
    let stays = |child: &str, parent: &str| {
        trees.reverted().contains(child)
            && (trees.remote().live(child)).is_some_and(|remote| remote.parent == parent)
    };
    let mut below = vec![id];
    while let Some(parent) = below.pop() {
        for child in trees.synced_children(parent) {
            match trees.local().get(child) {
                Some(node) if node.shape.parent == *parent || stays(child, parent) => return false,
                Some(_) => {}
                None => below.push(child),
            }
        }
    }
    true
}

/// The bytes a change record of a node with this shape takes, roughly.
fn record_cost(shape: Option<&Shape>) -> usize {
    RECORD_BYTES
        + shape.map_or(0, |shape| {
            shape.name.len() + shape.content.as_ref().map_or(0, |c| c.encoded_len())
        })
}

/// The bytes of change records `want` makes, roughly: one record for each node it changes, and a
/// record carries the node's current content.
fn cost(trees: &Trees, want: &Want) -> usize {
    match want {
        Want::Create {
            shape, pending_for, ..
        } => record_cost(Some(shape)) + marked(pending_for),
        Want::Move {
            id,
            name,
            pending_for,
            ..
        } => {
            record_cost(trees.remote().live(id).or(trees.synced().live(id)))
                + name.len()
                + marked(pending_for)
        }
        Want::Content { content, .. } => RECORD_BYTES + content.encoded_len(),
        Want::Delete { members, .. } => members
            .iter()
            .map(|node| record_cost(trees.synced().live(node)))
            .sum(),
        Want::Retrash { members, .. } => members
            .iter()
            .map(|node| 2 * record_cost(trees.remote().get(node).map(|node| &node.shape)))
            .sum(),
        Want::Restore { cost, .. } => *cost,
    }
}

fn marked(pending_for: &Option<String>) -> usize {
    pending_for.as_ref().map_or(0, |name| 2 * name.len())
}

enum Want {
    Create {
        id: NodeId,
        shape: Shape,
        pending_for: Option<String>,
    },
    Move {
        id: NodeId,
        parent: NodeId,
        name: String,
        guarded: bool,
        pending_for: Option<String>,
    },
    Content {
        id: NodeId,
        content: Content,
    },
    Delete {
        id: NodeId,
        members: BTreeSet<NodeId>,
    },
    /// Trashing again the nodes under `id` that an answer's op brought back from the trash with
    /// their trash group (R8) outside what the person answered (section 6.4): a move of each to
    /// where it is, so that the commit has seen its placement, and a delete of `id`.
    Retrash {
        id: NodeId,
        members: BTreeSet<NodeId>,
    },
    /// A restore a person asked for, with the bytes of change records it makes, roughly: for a
    /// node R holds as trashed, a record for every node trashed with it, and otherwise one.
    Restore {
        op: proto::Restore,
        cost: usize,
    },
}

impl Want {
    /// The nodes a want other than a restore changes. A restore of one of them waits for it.
    fn nodes(&self) -> impl Iterator<Item = &NodeId> {
        let (one, many) = match self {
            Want::Create { id, .. } | Want::Move { id, .. } | Want::Content { id, .. } => {
                (Some(id), None)
            }
            Want::Delete { members, .. } | Want::Retrash { members, .. } => (None, Some(members)),
            Want::Restore { .. } => (None, None),
        };
        one.into_iter().chain(many.into_iter().flatten())
    }

    fn ops(&self) -> usize {
        match self {
            Want::Retrash { members, .. } => members.len() + 1,
            _ => 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    Valid,
    NameTaken,
    Cycle,
    Wait,
    Merged,
}

type Planned = (
    Vec<proto::Op>,
    BTreeMap<NodeId, Vec<NodeId>>,
    Vec<NodeId>,
    Vec<NodeId>,
);

#[allow(clippy::too_many_arguments)]
fn plan(
    trees: &Trees,
    candidates: &[&NodeId],
    bound: Option<(usize, &str)>,
    skip: &BTreeSet<NodeId>,
    problems: &BTreeSet<NodeId>,
    needed: &BTreeSet<NodeId>,
    cap: usize,
    answered: &[proto::Op],
    split: &BTreeSet<NodeId>,
    aliases: &BTreeMap<NodeId, NodeId>,
    restores: &BTreeMap<RestoreId, RestoreIntent>,
    answers: &BTreeSet<NodeId>,
    device_id: &str,
) -> (Planned, bool) {
    let mut wants = Vec::new();
    if !restores.is_empty() {
        let mut groups: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, node) in trees.remote().iter().filter(|(_, node)| node.trashed) {
            *groups.entry(&node.trash_group).or_default() += record_cost(Some(&node.shape));
        }
        for intent in restores.values() {
            let cost = match trees.remote().get(intent.node()) {
                Some(node) if node.trashed && intent.version_id().is_none() => {
                    groups[node.trash_group.as_str()]
                }
                node => record_cost(node.map(|node| &node.shape)),
            };
            wants.push(Want::Restore {
                op: intent.op().clone(),
                cost,
            });
        }
    }
    let mut deleted = BTreeSet::new();
    let mut reverts = Vec::new();
    let mut cycle_reverts = Vec::new();
    let mut within = None;
    for &id in candidates.iter().filter(|id| !skip.contains(**id)) {
        if within.is_none() && bound.is_some_and(|(_, bound)| id.as_str() >= bound) {
            within = Some(wants.len());
        }
        let class = trees.acknowledged_class(id);
        if !matches!(class, Class::Local | Class::Concurrent) {
            continue;
        }
        let synced = trees.acknowledged(id);
        let Some(local) = trees.local().get(id).map(|node| &node.shape) else {
            let kept_settled =
                (trees.kept_original(id)).is_none_or(|kept| trees.class(kept) == Class::InSync);
            if kept_settled && deletes(trees, id, needed) {
                deleted.insert(id.clone());
            }
            continue;
        };
        let aliased;
        let in_alias = aliases.contains_key(&local.parent);
        let local = match aliases.get(&local.parent) {
            Some(into) => {
                aliased = Shape {
                    parent: into.clone(),
                    ..local.clone()
                };
                &aliased
            }
            None => local,
        };
        let Some(remote) = trees.remote().get(id) else {
            if !trees.reverted().contains(id) && !trees.is_discarded(id) {
                let shape = match trees.lost_position(id) {
                    Some(at) => Shape {
                        parent: aliases.get(&at.parent).unwrap_or(&at.parent).clone(),
                        name: at.name.clone(),
                        ..local.clone()
                    },
                    None => local.clone(),
                };
                let pending_for = (trees.lost_position(id).is_none())
                    .then(|| trees.create_mark(id))
                    .flatten()
                    .map(str::to_owned);
                wants.push(Want::Create {
                    id: id.clone(),
                    shape,
                    pending_for,
                });
            }
            continue;
        };
        if remote.shape.kind != local.kind || synced.is_some_and(|s| s.kind != local.kind) {
            continue;
        }
        let remote = trees.remote().live(id);
        let placed = |shape: Option<&Shape>| shape.is_some_and(|s| s.same_position(local));
        let agreed = synced.filter(|_| !trees.unsent_moves().contains(id));
        if in_alias && (placed(agreed) || placed(remote)) && !trees.reverted().contains(id) {
            reverts.push(id.clone());
        }
        if (synced.is_none_or(|s| !s.same_position(local)) || trees.unsent_moves().contains(id))
            && remote.is_none_or(|r| !r.same_position(local))
            && !trees.reverted().contains(id)
            && !trees.reviving(id)
        {
            wants.push(Want::Move {
                id: id.clone(),
                parent: local.parent.clone(),
                name: local.name.clone(),
                guarded: trees.resends().contains(id)
                    || (trees.remote().get(id)).is_some_and(|node| {
                        !node.trashed
                            && node.pending_by.as_deref() == Some(device_id)
                            && node.shape.parent == local.parent
                            && (node.pending_for.as_deref())
                                .is_some_and(|mark| nfc(mark) == nfc(&local.name))
                    })
                    || (trees.pending_finishes().get(id)).is_some_and(|finish| {
                        finish.parent == local.parent && nfc(&finish.name) == nfc(&local.name)
                    }),
                pending_for: None,
            });
        }
        let differs = |shape: Option<&Shape>, content: &Content| {
            shape
                .and_then(|shape| shape.content.as_ref())
                .is_none_or(|other| !same_content(other, content))
        };
        if let Some(content) = &local.content
            && ((differs(synced, content) && differs(remote, content))
                || trees.unsent_edits().contains(id))
        {
            wants.push(Want::Content {
                id: id.clone(),
                content: content.clone(),
            });
        }
    }
    let within = within.unwrap_or(wants.len());
    let mut scratch = Scratch::new(trees);
    let mut tops = Vec::new();
    let mut children: BTreeMap<&str, Vec<&NodeId>> = BTreeMap::new();
    for id in &deleted {
        match trees.synced().live(id).map(|s| &s.parent) {
            Some(parent) if deleted.contains(parent) => {
                children.entry(parent).or_default().push(id)
            }
            _ => tops.push(id),
        }
    }
    while let Some(id) = tops.pop() {
        let mut members = BTreeSet::new();
        let mut below = vec![id];
        while let Some(node) = below.pop() {
            members.insert(node.clone());
            below.extend(children.get(node.as_str()).into_iter().flatten());
        }
        let bytes: usize = members
            .iter()
            .map(|node| record_cost(trees.synced().live(node)))
            .sum();
        let kept = (scratch.subtree(id).iter())
            .any(|node| trees.excluded(node) || trees.kept_original(node).is_some());
        match children.get(id.as_str()) {
            Some(kids) if kept || bytes > MAX_COMMIT_BYTES || split.contains(id) => {
                tops.extend(kids)
            }
            _ if kept => {}
            _ => wants.push(Want::Delete {
                id: id.clone(),
                members,
            }),
        }
    }

    let (creates, deferred) = (trees.dirty_creates(), Cell::new(false));
    loop {
        let planned: BTreeMap<&NodeId, &NodeId> = (wants.iter())
            .filter_map(|want| match want {
                Want::Create { id, shape, .. } => Some((id, &shape.parent)),
                _ => None,
            })
            .collect();
        let placed = |id: &NodeId| {
            let mut parent = planned.get(id).copied();
            for _ in 0..=planned.len() + creates.len() {
                let Some(at) = parent else {
                    return false;
                };
                if *at == trees.local().root
                    || trees.remote().live(at).is_some()
                    || (skip.contains(at) && !trees.excluded(at))
                    || (creates.contains(at) && trees.held(at) && !trees.excluded(at))
                {
                    return true;
                }
                if at == id {
                    return false;
                }
                parent = planned.get(at).copied().or_else(|| {
                    let local = (trees.local().get(at))
                        .filter(|_| creates.contains(at) && !trees.excluded(at))?;
                    deferred.set(deferred.get() || bound.is_some());
                    Some(
                        trees
                            .lost_position(at)
                            .map_or(&local.shape.parent, |lost| &lost.parent),
                    )
                });
            }
            false
        };
        let redirects: BTreeMap<NodeId, (NodeId, String)> = (wants.iter())
            .filter_map(|want| {
                let Want::Create { id, shape, .. } = want else {
                    return None;
                };
                let local = (trees.local().get(id))
                    .filter(|_| trees.lost_position(id).is_some() && !placed(id))?;
                let parent = aliases
                    .get(&local.shape.parent)
                    .unwrap_or(&local.shape.parent);
                (shape.parent != *parent || shape.name != local.shape.name)
                    .then(|| (id.clone(), (parent.clone(), local.shape.name.clone())))
            })
            .collect();
        let ancestors = |id: &NodeId| {
            let mut chain: Vec<&NodeId> = Vec::new();
            let mut at = planned.get(id);
            while let Some(&parent) = at
                && chain.len() < planned.len()
            {
                chain.push(parent);
                at = planned.get(parent);
            }
            chain
        };
        let below_another = |id: &NodeId| {
            (ancestors(id).into_iter().take_while(|at| *at != id))
                .any(|at| redirects.contains_key(at))
        };
        let on_cycle = |id: &NodeId| ancestors(id).contains(&id);
        let Some((id, (parent, name))) = (redirects.iter())
            .find(|(id, _)| !below_another(id))
            .or_else(|| redirects.iter().find(|(id, _)| on_cycle(id)))
            .map(|(id, at)| (id.clone(), at.clone()))
        else {
            break;
        };
        if let Some(Want::Create {
            shape, pending_for, ..
        }) = (wants.iter_mut())
            .find(|want| matches!(want, Want::Create { id: create, .. } if *create == id))
        {
            shape.parent = parent;
            shape.name = name;
            *pending_for = trees.create_mark(&id).map(str::to_owned);
        }
    }
    scratch.creating = (wants.iter())
        .filter_map(|want| match want {
            Want::Create { id, shape, .. } => Some((id.clone(), shape.parent.clone())),
            _ => None,
        })
        .collect();
    let restored: BTreeSet<&str> = restores.values().map(RestoreIntent::node).collect();
    for node in wants.iter().flat_map(Want::nodes) {
        if restored.contains(node.as_str()) {
            *scratch.unsent.entry(node.clone()).or_default() += 1;
        }
    }
    let unsettled = |node: &&str| {
        skip.contains(*node)
            || trees.waiting().contains_key(*node)
            || trees.class(node) == Class::Same
    };
    let (unsettled, settled): (Vec<&str>, Vec<&str>) = restored.into_iter().partition(unsettled);
    for node in unsettled {
        *scratch.unsent.entry(node.to_owned()).or_default() += 1;
    }
    scratch.restoring = settled.into_iter().map(str::to_owned).collect();
    let mut moving: BTreeSet<NodeId> = wants
        .iter()
        .filter_map(|want| match want {
            Want::Move { id, .. } => Some(id.clone()),
            _ => None,
        })
        .chain(trees.held_finishes().cloned())
        .collect();
    let mut wants: Vec<Option<Want>> = wants.into_iter().map(Some).collect();
    let mut ops = Vec::new();
    let mut deletes = BTreeMap::new();
    let removed = |id: &NodeId| {
        trees.local().get(id).is_none()
            && (trees.synced().get(id).is_some() || trees.is_removed_here(id))
    };
    let mut emit = |want: Want, scratch: &mut Scratch, moving: &mut BTreeSet<NodeId>| {
        let generated = matches!(want, Want::Retrash { .. });
        for node in want.nodes().filter(|_| !generated) {
            if let Some(left) = scratch.unsent.get_mut(node) {
                *left -= 1;
                if *left == 0 {
                    scratch.unsent.remove(node);
                }
            }
        }
        let op = match want {
            Want::Create {
                id,
                shape,
                pending_for,
            } => {
                if shape.kind == Kind::Dir {
                    let level = scratch.merge_level(&shape.parent);
                    scratch.protected.insert(level.clone());
                    scratch.merges.insert(id.clone(), level);
                }
                scratch.place(&id, &shape.parent, &shape.name, shape.kind);
                match shape.kind {
                    Kind::Dir => op::Op::CreateDir(proto::CreateDir {
                        node_id: id,
                        parent_id: shape.parent,
                        name: shape.name,
                        pending_for: pending_for.unwrap_or_default(),
                    }),
                    Kind::File => op::Op::CreateFile(proto::CreateFile {
                        node_id: id,
                        parent_id: shape.parent,
                        name: shape.name,
                        content: shape.content,
                    }),
                }
            }
            Want::Move {
                id,
                parent,
                name,
                guarded,
                pending_for,
            } => {
                moving.remove(&id);
                let from = guarded.then(|| proto::Place {
                    parent_id: scratch
                        .parent(&id)
                        .expect("moved nodes are in R")
                        .to_owned(),
                    name: scratch.name(&id).expect("moved nodes are in R").to_owned(),
                });
                let kind = scratch.kind(&id).expect("moved nodes are in R");
                let group = (scratch.group(&id))
                    .filter(|_| !scratch.live(&id))
                    .map(str::to_owned);
                scratch.place(&id, &parent, &name, kind);
                if let Some(group) = group {
                    let revived = scratch.revive_group(&id, &group);
                    scratch
                        .owed
                        .extend(revived.into_iter().filter(|id| removed(id)));
                }
                op::Op::Move(proto::Move {
                    node_id: id,
                    new_parent_id: parent,
                    new_name: name,
                    from,
                    pending_for: pending_for.unwrap_or_default(),
                })
            }
            Want::Content { id, content } => {
                scratch.revive(&id);
                op::Op::UpdateContent(proto::UpdateContent {
                    resend: trees.resends().contains(&id),
                    node_id: id,
                    content: Some(content),
                })
            }
            Want::Delete { id, members } => {
                scratch.trash(&id);
                deletes.insert(id.clone(), members.into_iter().collect());
                let requires = trees.kept_original(&id).map(|kept| {
                    let held = trees.remote().get(kept);
                    proto::NodeRevisions {
                        node_id: kept.clone(),
                        content_rev: held.map_or(0, |node| node.content_rev),
                        meta_rev: held.map_or(0, |node| node.meta_rev),
                    }
                });
                op::Op::Delete(proto::Delete {
                    node_id: id,
                    requires,
                })
            }
            Want::Retrash { id, members } => {
                for node in &members {
                    let shape = &trees.remote().get(node).expect("revived from R").shape;
                    let placed = op::Op::Move(proto::Move {
                        node_id: node.clone(),
                        new_parent_id: shape.parent.clone(),
                        new_name: shape.name.clone(),
                        from: None,
                        pending_for: String::new(),
                    });
                    ops.push(proto::Op { op: Some(placed) });
                }
                scratch.trash(&id);
                deletes.insert(id.clone(), members.into_iter().collect());
                op::Op::Delete(proto::Delete {
                    node_id: id,
                    requires: None,
                })
            }
            Want::Restore { op, .. } => {
                scratch.restoring.remove(&op.node_id);
                let trashed = (trees.remote().get(&op.node_id))
                    .filter(|node| node.trashed && op.version_id.is_empty());
                match trashed {
                    Some(node) => {
                        let position = &node.shape;
                        let parent = Some(&op.target_parent_id).filter(|id| !id.is_empty());
                        let parent = parent.unwrap_or(&position.parent);
                        let name = Some(&op.name).filter(|name| !name.is_empty());
                        let name = name.unwrap_or(&position.name);
                        let fit = scratch.fit_at(&op.node_id, parent, name);
                        if matches!(fit, Fit::Valid | Fit::NameTaken) {
                            scratch.place(&op.node_id, parent, name, node.shape.kind);
                            scratch.revive_group(&op.node_id, &node.trash_group);
                        }
                    }
                    None => scratch.revive(&op.node_id),
                }
                op::Op::Restore(op)
            }
        };
        ops.push(proto::Op { op: Some(op) });
    };
    let mut emitted = answered.len();
    let mut bytes = (answered.iter().filter_map(|op| op.op.as_ref()))
        .map(|op| match op {
            op::Op::UpdateContent(update) => {
                RECORD_BYTES + update.content.as_ref().map_or(0, |c| c.encoded_len())
            }
            op => record_cost(trees.synced().live(op_node(op))),
        })
        .sum();
    let fits = |emitted: usize, bytes: usize, ops: usize, cost: usize| {
        emitted + ops <= cap && (emitted == 0 || bytes + cost <= MAX_COMMIT_BYTES)
    };
    let priced =
        |scratch: &Scratch, want: &Want| cost(trees, want) + scratch.revival(want, answers, trees);
    let room = |emitted: &mut usize, bytes: &mut usize, scratch: &Scratch, want: &Want| {
        let cost = priced(scratch, want);
        if !fits(*emitted, *bytes, want.ops(), cost) {
            return false;
        }
        *emitted += want.ops();
        *bytes += cost;
        true
    };
    let ordered = !(wants.iter().flatten())
        .any(|want| matches!(want, Want::Delete { .. } | Want::Restore { .. }));
    let (mut pass, mut stalled) = (1, false);
    let mut filled_at = None;
    let capped = loop {
        let (mut progress, mut full) = (false, false);
        for at in 0..wants.len() {
            let Some(want) =
                (wants[at].as_ref()).filter(|want| scratch.fit(want, &moving) == Fit::Valid)
            else {
                continue;
            };
            let Some(bundle) = scratch.compensation(want, answers, trees) else {
                continue;
            };
            let order = match want {
                Want::Create { shape, .. } if shape.kind == Kind::Dir => {
                    deletes_before_create(&wants, &scratch, &moving, &shape.parent)
                }
                _ => Vec::new(),
            };
            for before in order {
                if !room(
                    &mut emitted,
                    &mut bytes,
                    &scratch,
                    wants[before].as_ref().expect("checked"),
                ) {
                    full = true;
                    break;
                }
                emit(
                    wants[before].take().expect("checked"),
                    &mut scratch,
                    &mut moving,
                );
                progress = true;
            }
            if full {
                filled_at = (!stalled).then_some((pass, at));
                break;
            }
            let want = wants[at].as_ref().expect("checked");
            let ops = want.ops() + bundle.iter().map(Want::ops).sum::<usize>();
            let cost = priced(&scratch, want)
                + bundle.iter().map(|other| cost(trees, other)).sum::<usize>();
            let alone = emitted == 0 && !bundle.is_empty();
            if !alone && !fits(emitted, bytes, ops, cost) {
                full = true;
                filled_at = (!stalled).then_some((pass, at));
                break;
            }
            emitted += ops;
            bytes += cost;
            emit(
                wants[at].take().expect("checked"),
                &mut scratch,
                &mut moving,
            );
            for retrash in bundle {
                emit(retrash, &mut scratch, &mut moving);
            }
            progress = true;
        }
        if full {
            break true;
        }
        pass += 1;
        if progress {
            continue;
        }
        stalled = true;
        if let Some(cycle) = name_cycle(&wants, &scratch, &moving) {
            let Some(Want::Move {
                id,
                parent,
                name,
                guarded,
                ..
            }) = &wants[cycle[0]]
            else {
                unreachable!("a name cycle holds moves");
            };
            let aside = Want::Move {
                id: id.clone(),
                parent: parent.clone(),
                name: scratch.free_name(parent, name),
                pending_for: Some(name.clone()),
                guarded: *guarded,
            };
            let all: usize = (cycle.iter())
                .filter_map(|at| wants[*at].as_ref())
                .map(|want| priced(&scratch, want))
                .sum();
            let whole = fits(
                emitted,
                bytes,
                cycle.len() + 1,
                all + priced(&scratch, &aside),
            );
            let bundled = scratch
                .compensation(&aside, answers, trees)
                .is_none_or(|bundle| !bundle.is_empty());
            if bundled
                || (emitted > 0 && !whole)
                || !room(&mut emitted, &mut bytes, &scratch, &aside)
            {
                break true;
            }
            let id = id.clone();
            let unsent = scratch.unsent.get(&id).copied();
            emit(aside, &mut scratch, &mut moving);
            moving.insert(id.clone());
            if let Some(unsent) = unsent {
                scratch.unsent.insert(id, unsent);
            }
            continue;
        }
        if let Some(at) = return_cycle(&wants, &scratch, &moving) {
            if !room(
                &mut emitted,
                &mut bytes,
                &scratch,
                wants[at].as_ref().expect("checked"),
            ) {
                break true;
            }
            emit(
                wants[at].take().expect("checked"),
                &mut scratch,
                &mut moving,
            );
            continue;
        }
        let held: Vec<NodeId> = (wants.iter().flatten())
            .filter_map(|want| match want {
                Want::Delete { id, .. } if scratch.fit(want, &moving) == Fit::Merged => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        let forced = wants.iter_mut().find_map(|slot| {
            let want = slot.as_ref()?;
            if scratch.fit(want, &moving) != Fit::NameTaken
                || (scratch.name_holders(want)).any(|holder| {
                    scratch.owed.contains(holder)
                        || held.iter().any(|id| scratch.inside(holder, id))
                })
            {
                return None;
            }
            let bundle = scratch.compensation(want, answers, trees)?;
            Some((slot, bundle))
        });
        let Some((slot, bundle)) = forced else {
            break false;
        };
        let want = slot.as_ref().expect("checked");
        let ops = want.ops() + bundle.iter().map(Want::ops).sum::<usize>();
        let cost =
            priced(&scratch, want) + bundle.iter().map(|other| cost(trees, other)).sum::<usize>();
        if !(emitted == 0 && !bundle.is_empty()) && !fits(emitted, bytes, ops, cost) {
            break true;
        }
        emitted += ops;
        bytes += cost;
        if let Some(Want::Create {
            shape, pending_for, ..
        }) = slot.as_mut()
            && shape.kind == Kind::Dir
            && (scratch.holders(&shape.parent, &shape.name)).any(|other| {
                moving.contains(other) || leaving_unsent(&scratch, aliases, problems, other)
            })
        {
            let name = scratch.free_name(&shape.parent, &shape.name);
            pending_for.get_or_insert_with(|| shape.name.clone());
            shape.name = name;
        }
        emit(slot.take().expect("checked"), &mut scratch, &mut moving);
        for retrash in bundle {
            emit(retrash, &mut scratch, &mut moving);
        }
    };
    if !capped {
        for want in wants.iter().flatten() {
            if let Want::Move { id, .. } = want
                && scratch.fit(want, &moving) == Fit::Cycle
            {
                cycle_reverts.push(id.clone());
            }
        }
        if cfg!(debug_assertions) && bound.is_none() {
            let wedged = wedged(trees, &wants, &scratch, &moving, skip);
            assert!(
                wedged.is_empty(),
                "planned creates or moves wait on a parent that neither the server holds, nor the plan places, nor a pending change brings: {wedged:?}"
            );
        }
    }
    let exact = !deferred.get()
        && bound.is_none_or(|(last, _)| {
            ordered && filled_at.is_some_and(|filled| filled < (last, within))
        });
    ((ops, deletes, reverts, cycle_reverts), exact)
}

fn remote_below(trees: &Trees, id: &str, ancestors: &BTreeSet<&str>) -> bool {
    let mut current = id;
    for _ in 0..=trees.remote().len() {
        let Some(node) = trees.remote().get(current) else {
            return false;
        };
        if ancestors.contains(node.shape.parent.as_str()) {
            return true;
        }
        current = &node.shape.parent;
    }
    false
}

fn deletes_before_create(
    wants: &[Option<Want>],
    scratch: &Scratch,
    moving: &BTreeSet<NodeId>,
    parent: &str,
) -> Vec<usize> {
    let reached: Vec<(usize, &Want, &NodeId)> = (wants.iter().enumerate())
        .filter_map(|(at, slot)| match slot {
            Some(want @ Want::Delete { id, .. }) => Some((at, want, id)),
            _ => None,
        })
        .filter(|(_, _, id)| scratch.merge_reaches(parent, id))
        .collect();
    if reached.is_empty() {
        return Vec::new();
    }
    let placing: Vec<&NodeId> = (wants.iter().flatten())
        .filter_map(|want| match want {
            Want::Create { shape, .. } => Some(&shape.parent),
            Want::Move { parent, .. } => Some(parent),
            _ => None,
        })
        .collect();
    (reached.into_iter())
        .filter(|(_, want, id)| {
            !placing.iter().any(|to| scratch.inside(to, id))
                && scratch.fit(want, moving) == Fit::Valid
        })
        .map(|(at, ..)| at)
        .collect()
}

/// A cycle of moves that each wait only for a name held by the node of the next one, as in a swap
/// or a rotation through a third name, by their indices in `wants`.
fn name_cycle(
    wants: &[Option<Want>],
    scratch: &Scratch,
    moving: &BTreeSet<NodeId>,
) -> Option<Vec<usize>> {
    let mut blocked: BTreeMap<&str, usize> = BTreeMap::new();
    let mut returns: BTreeMap<&str, &str> = BTreeMap::new();
    for (at, slot) in wants.iter().enumerate() {
        let (want, node) = match slot {
            Some(want @ Want::Move { id, .. }) => (want, id),
            Some(want @ Want::Restore { op, .. }) => (want, &op.node_id),
            _ => continue,
        };
        if let Some(returning) = scratch.awaited_return(want, moving) {
            returns.insert(node, returning);
        } else if matches!(want, Want::Move { .. }) && scratch.fit(want, moving) == Fit::NameTaken {
            blocked.insert(node, at);
        }
    }
    for &start in blocked.keys() {
        let mut cycle = Vec::new();
        let mut seen = BTreeSet::new();
        let mut node = start;
        while seen.insert(node) {
            if let Some(&at) = blocked.get(node) {
                cycle.push(at);
                let Some(Want::Move {
                    id, parent, name, ..
                }) = &wants[at]
                else {
                    unreachable!("indexed from the moves");
                };
                let holders: Vec<&NodeId> = (scratch.holders(parent, name))
                    .filter(|holder| *holder != id)
                    .collect();
                let [holder] = holders.as_slice() else {
                    break;
                };
                node = holder;
            } else if let Some(&returning) = returns.get(node) {
                node = returning;
            } else {
                break;
            }
            if node == start {
                return Some(cycle);
            }
        }
    }
    None
}

fn return_cycle(
    wants: &[Option<Want>],
    scratch: &Scratch,
    moving: &BTreeSet<NodeId>,
) -> Option<usize> {
    let awaits: Vec<(usize, &str, &str)> = (wants.iter().enumerate())
        .filter_map(|(at, slot)| {
            let want = slot.as_ref()?;
            let node = match want {
                Want::Move { id, .. } => id,
                Want::Restore { op, .. } => &op.node_id,
                _ => return None,
            };
            Some((at, node.as_str(), scratch.awaited_return(want, moving)?))
        })
        .collect();
    let next: BTreeMap<&str, &str> = (awaits.iter())
        .map(|&(_, node, returning)| (node, returning))
        .collect();
    awaits.iter().find_map(|&(at, start, returning)| {
        let sendable = (wants[at].as_ref()).is_some_and(|want| {
            matches!(scratch.placement(want, moving), Fit::Valid | Fit::NameTaken)
        });
        let mut seen = BTreeSet::from([start]);
        let mut node = returning;
        while seen.insert(node) {
            let Some(&after) = next.get(node) else {
                break;
            };
            node = after;
        }
        (sendable && node == start).then_some(at)
    })
}

#[derive(Clone)]
struct Entry {
    parent: NodeId,
    name: String,
    key: String,
    kind: Kind,
    live: bool,
    group: Option<String>,
}

#[derive(Clone)]
struct Scratch<'a> {
    trees: &'a Trees,
    nodes: BTreeMap<NodeId, Entry>,
    children: BTreeMap<NodeId, BTreeMap<String, BTreeSet<NodeId>>>,
    /// For each folder this commit creates, the nearest folder above it that the commit does not
    /// create and how many levels below that folder it is. The server may merge it into any folder
    /// at that level (MERGED_INTO), since it merges by the names it holds when the commit applies,
    /// and another device may have renamed one of them since R. Deleting one in the same commit
    /// would trash what the create brings, so their deletes wait for the next commit.
    merges: BTreeMap<NodeId, (NodeId, usize)>,
    protected: BTreeSet<(NodeId, usize)>,
    pinned: BTreeMap<NodeId, BTreeSet<(NodeId, usize)>>,
    /// For each node a queued restore names, how many wants that change it are not emitted yet,
    /// and one more while the node is not settled: this build leaves it out, so its changes are
    /// not among the wants, or S has not taken what L and R agree on. The restore waits for all
    /// of them, so that it is applied to what this device sent and S shows what the device
    /// holds.
    unsent: BTreeMap<NodeId, usize>,
    restoring: BTreeSet<NodeId>,
    owed: BTreeSet<NodeId>,
    creating: BTreeMap<NodeId, NodeId>,
}

impl<'a> Scratch<'a> {
    fn new(trees: &'a Trees) -> Self {
        Self {
            trees,
            nodes: BTreeMap::new(),
            children: BTreeMap::new(),
            merges: BTreeMap::new(),
            protected: BTreeSet::new(),
            pinned: BTreeMap::new(),
            unsent: BTreeMap::new(),
            restoring: BTreeSet::new(),
            owed: BTreeSet::new(),
            creating: BTreeMap::new(),
        }
    }

    fn root(&self) -> &'a str {
        &self.trees.remote().root
    }

    fn parent(&self, id: &str) -> Option<&str> {
        match self.nodes.get(id) {
            Some(entry) => Some(&entry.parent),
            None => (self.trees.remote().get(id)).map(|node| node.shape.parent.as_str()),
        }
    }

    fn name(&self, id: &str) -> Option<&str> {
        match self.nodes.get(id) {
            Some(entry) => Some(&entry.name),
            None => (self.trees.remote().get(id)).map(|node| node.shape.name.as_str()),
        }
    }

    fn kind(&self, id: &str) -> Option<Kind> {
        match self.nodes.get(id) {
            Some(entry) => Some(entry.kind),
            None => (self.trees.remote().get(id)).map(|node| node.shape.kind),
        }
    }

    fn live(&self, id: &str) -> bool {
        match self.nodes.get(id) {
            Some(entry) => entry.live,
            None => self.trees.remote().live(id).is_some(),
        }
    }

    fn group(&self, id: &str) -> Option<&str> {
        match self.nodes.get(id) {
            Some(entry) => entry.group.as_deref().filter(|_| !entry.live),
            None => (self.trees.remote().get(id))
                .filter(|node| node.trashed)
                .map(|node| node.trash_group.as_str()),
        }
    }

    fn in_trash(&self, id: &str) -> bool {
        self.parent(id).is_some() && !self.live(id)
    }

    fn entry(&mut self, id: &str) -> &mut Entry {
        if !self.nodes.contains_key(id) {
            let node = self
                .trees
                .remote()
                .get(id)
                .expect("scratch nodes are in R or placed");
            let entry = Entry {
                parent: node.shape.parent.clone(),
                name: node.shape.name.clone(),
                key: name_key(&node.shape.name),
                kind: node.shape.kind,
                live: !node.trashed,
                group: node.trashed.then(|| node.trash_group.clone()),
            };
            self.nodes.insert(id.to_owned(), entry);
        }
        self.nodes.get_mut(id).expect("inserted above")
    }

    fn link(&mut self, id: &str) {
        let entry = self.entry(id);
        entry.live = true;
        let (parent, key) = (entry.parent.clone(), entry.key.clone());
        (self.children.entry(parent).or_default())
            .entry(key)
            .or_default()
            .insert(id.to_owned());
    }

    fn unlink(&mut self, id: &str) {
        if !self.live(id) {
            return;
        }
        let entry = self.entry(id);
        entry.live = false;
        let (parent, key) = (entry.parent.clone(), entry.key.clone());
        if let Some(named) = (self.children.get_mut(&parent)).and_then(|keys| keys.get_mut(&key)) {
            named.remove(id);
        }
    }

    fn fit(&self, want: &Want, moving: &BTreeSet<NodeId>) -> Fit {
        match self.awaited_return(want, moving) {
            Some(_) => Fit::Wait,
            None => self.placement(want, moving),
        }
    }

    fn placement(&self, want: &Want, moving: &BTreeSet<NodeId>) -> Fit {
        match want {
            Want::Create { id, shape, .. } => self.fit_at(id, &shape.parent, &shape.name),
            Want::Move {
                id, parent, name, ..
            } => self.fit_at(id, parent, name),
            Want::Content { id, .. } if moving.contains(id) => Fit::Wait,
            Want::Content { .. } => Fit::Valid,
            Want::Delete { id, members } | Want::Retrash { id, members } => {
                let subtree = self.subtree(id);
                if !subtree.iter().all(|node| members.contains(node)) {
                    Fit::Wait
                } else if self.protects(id) || subtree.iter().any(|node| self.protects(node)) {
                    Fit::Merged
                } else {
                    Fit::Valid
                }
            }
            Want::Restore { op, .. } if self.unsent.contains_key(&op.node_id) => Fit::Wait,
            Want::Restore { .. } => Fit::Valid,
        }
    }

    fn awaited_return<'s>(&'s self, want: &'s Want, moving: &BTreeSet<NodeId>) -> Option<&'s str> {
        let trashed_parent = |id: &str| self.parent(id).filter(|_| !self.live(id));
        let revived = match want {
            Want::Create { shape, .. } => Some(shape.parent.as_str()),
            Want::Move { id, parent, .. } => {
                Some(parent.as_str()).filter(|to| !self.inside(to, id))
            }
            Want::Content { id, .. } => trashed_parent(id),
            Want::Restore { op, .. } => {
                let target = Some(op.target_parent_id.as_str())
                    .filter(|target| !target.is_empty() && op.version_id.is_empty());
                trashed_parent(&op.node_id)
                    .map(|parent| target.unwrap_or(parent))
                    .filter(|to| !self.inside(to, &op.node_id))
            }
            Want::Delete { .. } | Want::Retrash { .. } => None,
        };
        revived.and_then(|id| self.returning_from_trash(id, moving))
    }

    fn returning_from_trash<'s>(
        &'s self,
        id: &'s str,
        moving: &BTreeSet<NodeId>,
    ) -> Option<&'s str> {
        let mut current = id;
        for _ in 0..=self.trees.remote().len() + self.nodes.len() {
            if self.live(current) {
                return None;
            }
            let parent = self.parent(current)?;
            if self.restoring.contains(current)
                || (moving.contains(current) && self.in_trash(parent))
            {
                return Some(current);
            }
            current = parent;
        }
        None
    }

    /// Whether `id` can take `name` under `parent`. A trashed parent counts: the server revives
    /// it (R8).
    fn fit_at(&self, id: &str, parent: &str, name: &str) -> Fit {
        if self.inside(parent, id) {
            return Fit::Cycle;
        }
        if parent != self.root() && self.kind(parent) != Some(Kind::Dir) {
            return Fit::Wait;
        }
        let taken = self.holders(parent, name).any(|other| other != id);
        if taken { Fit::NameTaken } else { Fit::Valid }
    }

    /// True when `ancestor` is `id` or holds it.
    fn inside(&self, id: &str, ancestor: &str) -> bool {
        let mut current = id;
        for _ in 0..=self.trees.remote().len() + self.nodes.len() + self.creating.len() {
            if current == ancestor {
                return true;
            }
            match (self.parent(current), self.creating.get(current)) {
                (Some(parent), _) => current = parent,
                (None, Some(parent)) => current = parent,
                (None, None) => return false,
            }
        }
        false
    }

    fn children_of<'s>(&'s self, parent: &'s str) -> impl Iterator<Item = &'s NodeId> + 's {
        (self.trees.remote_children(parent))
            .filter(|id| !self.nodes.contains_key(*id))
            .chain(
                self.children
                    .get(parent)
                    .into_iter()
                    .flat_map(|keys| keys.values().flatten()),
            )
    }

    /// The live nodes under `id`.
    fn subtree(&self, id: &str) -> Vec<NodeId> {
        let mut found = Vec::new();
        let mut stack = vec![id.to_owned()];
        while let Some(current) = stack.pop() {
            for child in self.children_of(&current) {
                found.push(child.clone());
                stack.push(child.clone());
            }
        }
        found
    }

    fn place(&mut self, id: &str, parent: &str, name: &str, kind: Kind) {
        if !self.protected.is_empty() {
            let leaving: Vec<(NodeId, Vec<(NodeId, usize)>)> = (std::iter::once(id.to_owned())
                .chain(self.subtree(id)))
            .map(|node| {
                let levels = self.protected_levels(&node);
                (node, levels)
            })
            .filter(|(_, levels)| !levels.is_empty())
            .collect();
            for (node, levels) in leaving {
                self.pinned.entry(node).or_default().extend(levels);
            }
        }
        self.unlink(id);
        let entry = Entry {
            parent: parent.to_owned(),
            name: name.to_owned(),
            key: name_key(name),
            kind,
            live: false,
            group: None,
        };
        self.nodes.insert(id.to_owned(), entry);
        self.link(id);
        self.revive(parent);
    }

    fn revive_group(&mut self, id: &str, group: &str) -> Vec<NodeId> {
        let trees = self.trees;
        let mut revived = Vec::new();
        let mut parents = vec![id.to_owned()];
        while let Some(parent) = parents.pop() {
            for child in trees.remote_trashed_children(&parent) {
                if self.group(child) != Some(group) {
                    continue;
                }
                self.link(child);
                parents.push(child.clone());
                revived.push(child.clone());
            }
        }
        revived
    }

    /// Makes `id` live with the trashed ancestors it needs, as R8 does.
    fn revive(&mut self, id: &str) {
        let mut current = id.to_owned();
        for _ in 0..=self.trees.remote().len() + self.nodes.len() {
            if self.live(&current) {
                return;
            }
            let Some(parent) = self.parent(&current).map(str::to_owned) else {
                return;
            };
            self.link(&current);
            current = parent;
        }
    }

    fn name_holders<'s>(&'s self, want: &'s Want) -> impl Iterator<Item = &'s NodeId> + 's {
        let (parent, name) = match want {
            Want::Create { shape, .. } => (shape.parent.as_str(), shape.name.as_str()),
            Want::Move { parent, name, .. } => (parent.as_str(), name.as_str()),
            _ => ("", ""),
        };
        self.holders(parent, name)
    }

    /// The live nodes named `name` under `parent`.
    fn holders<'s>(
        &'s self,
        parent: &str,
        name: &str,
    ) -> impl Iterator<Item = &'s NodeId> + use<'s, 'a> {
        let key = name_key(name);
        let placed = (self.children.get(parent)).and_then(|keys| keys.get(&key));
        (self.trees.remote_holders_of_key(parent, &key))
            .filter(|id| !self.nodes.contains_key(*id))
            .chain(placed.into_iter().flatten())
    }

    fn merge_level(&self, parent: &str) -> (NodeId, usize) {
        match self.merges.get(parent) {
            Some((anchor, depth)) => (anchor.clone(), depth + 1),
            None => (parent.to_owned(), 1),
        }
    }

    fn protects(&self, id: &str) -> bool {
        self.live(id) && self.kind(id) == Some(Kind::Dir) && !self.protected_levels(id).is_empty()
    }

    fn protected_levels(&self, id: &str) -> Vec<(NodeId, usize)> {
        let mut found = Vec::new();
        let mut current = id;
        for depth in 0..=self.trees.remote().len() + self.nodes.len() {
            let pinned = (self.pinned.get(current).into_iter().flatten())
                .map(|(anchor, at)| (anchor.clone(), at + depth));
            found.extend(pinned.filter(|level| self.protected.contains(level)));
            let Some(parent) = self.parent(current) else {
                break;
            };
            let level = (parent.to_owned(), depth + 1);
            if self.protected.contains(&level) {
                found.push(level);
            }
            current = parent;
        }
        found
    }

    fn merge_reaches(&self, parent: &str, id: &str) -> bool {
        let (anchor, depth) = self.merge_level(parent);
        let below = |node: &str, at: usize| at <= depth && self.folder_below(node, depth - at);
        let pinned_at = |node: &str| {
            (self.pinned.get(node).into_iter().flatten())
                .filter(|(at_anchor, _)| *at_anchor == anchor)
                .map(|(_, at)| *at)
                .collect::<Vec<_>>()
        };
        if self.inside(&anchor, id) && below(&anchor, 0) {
            return true;
        }
        let inside = (self.pinned.keys()).filter(|node| self.inside(node, id));
        if inside
            .into_iter()
            .any(|node| pinned_at(node).into_iter().any(|at| below(node, at)))
        {
            return true;
        }
        let mut current = id;
        for up in 1..=depth {
            let Some(next) = self.parent(current) else {
                return false;
            };
            if (next == anchor && below(id, up))
                || pinned_at(next).into_iter().any(|at| below(id, at + up))
            {
                return true;
            }
            current = next;
        }
        false
    }

    fn folder_below(&self, folder: &str, levels: usize) -> bool {
        match levels {
            0 => self.live(folder) && self.kind(folder) == Some(Kind::Dir),
            _ => (self.children_of(folder)).any(|child| self.folder_below(child, levels - 1)),
        }
    }

    /// A name under `parent` that nothing holds, for a folder created while the node holding its
    /// name moves away: it is renamed to its name in a later commit, instead of being merged into
    /// a folder that leaves or renamed by the server beside a file that leaves.
    fn free_name(&self, parent: &str, name: &str) -> String {
        (1..)
            .map(|n| {
                let suffix = match n {
                    1 => " (pending)".to_owned(),
                    n => format!(" (pending {n})"),
                };
                let mut stem: Vec<char> = nfc(name).chars().collect();
                let named = |stem: &[char]| format!("{}{suffix}", stem.iter().collect::<String>());
                while nfc(&named(&stem)).len() > MAX_NAME_BYTES {
                    stem.pop();
                }
                named(&stem)
            })
            .find(|candidate| self.holders(parent, candidate).next().is_none())
            .expect("a free name")
    }

    fn trash(&mut self, id: &str) {
        for node in self.subtree(id).into_iter().chain([id.to_owned()]) {
            self.entry(&node).group = None;
            self.unlink(&node);
        }
    }

    fn revival(&self, want: &Want, answers: &BTreeSet<NodeId>, trees: &Trees) -> usize {
        let record = |id: &str| record_cost(trees.remote().get(id).map(|node| &node.shape));
        let (mut current, group) = match want {
            Want::Create { shape, .. } => (shape.parent.clone(), None),
            Want::Move { id, parent, .. } => {
                let group = self.group(id).filter(|_| answers.contains(id));
                (parent.clone(), group.map(|group| (id, group)))
            }
            Want::Content { id, .. } => match self.parent(id) {
                Some(parent) => (parent.to_owned(), None),
                None => return 0,
            },
            Want::Delete { .. } | Want::Retrash { .. } | Want::Restore { .. } => return 0,
        };
        let mut bytes = 0;
        for _ in 0..=self.trees.remote().len() + self.nodes.len() {
            if self.live(&current) {
                break;
            }
            let Some(parent) = self.parent(&current) else {
                break;
            };
            bytes += record(&current);
            current = parent.to_owned();
        }
        let Some((id, group)) = group else {
            return bytes;
        };
        let mut below = vec![id.as_str()];
        while let Some(parent) = below.pop() {
            for child in self.trees.remote_trashed_children(parent) {
                if self.group(child) == Some(group) {
                    bytes += record(child);
                    below.push(child);
                }
            }
        }
        bytes
    }

    /// The wants that must go into the same commit right after `want`, so that what it brings
    /// back from the trash with an answered node's trash group (R8) outside `answers` is trashed
    /// again (section 6.4), or none when one of them would not apply after it.
    fn compensation(
        &self,
        want: &Want,
        answers: &BTreeSet<NodeId>,
        trees: &Trees,
    ) -> Option<Vec<Want>> {
        let Want::Move {
            id, parent, name, ..
        } = want
        else {
            return Some(Vec::new());
        };
        let Some(group) = self.group(id).filter(|_| answers.contains(id)) else {
            return Some(Vec::new());
        };
        let mut scratch = self.clone();
        let kind = scratch.kind(id).expect("moved nodes are in R");
        scratch.place(id, parent, name, kind);
        let revived: BTreeSet<NodeId> = scratch.revive_group(id, group).into_iter().collect();
        let placed = |node: &str| match answers.contains(node) {
            true => trees
                .local()
                .get(node)
                .map(|local| local.shape.parent.as_str()),
            false => scratch.parent(node),
        };
        let mut needed = BTreeSet::new();
        for answered in revived.iter().filter(|node| answers.contains(*node)) {
            let mut at = placed(answered);
            while let Some(parent) = at.filter(|parent| revived.contains(*parent))
                && needed.insert(parent.to_owned())
            {
                at = placed(parent);
            }
        }
        let outside: BTreeSet<NodeId> = (revived.into_iter())
            .filter(|node| !answers.contains(node) && !needed.contains(node))
            .collect();
        let mut bundle = Vec::new();
        for top in &outside {
            if scratch
                .parent(top)
                .is_some_and(|parent| outside.contains(parent))
            {
                continue;
            }
            let members = (scratch.subtree(top).into_iter())
                .filter(|node| outside.contains(node))
                .chain([top.clone()])
                .collect();
            let retrash = Want::Retrash {
                id: top.clone(),
                members,
            };
            if scratch.fit(&retrash, &BTreeSet::new()) != Fit::Valid {
                return None;
            }
            scratch.trash(top);
            bundle.push(retrash);
        }
        Some(bundle)
    }
}
