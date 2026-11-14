// SPDX-License-Identifier: Apache-2.0

//! Applying remote changes to disk (docs/design/sync-engine.md sections 2.5, 5.2 and 5.3): the batch
//! of effects in flight for nodes whose only change is remote, parking for remote states that cannot
//! be reached one move at a time, and the effect counter of section 2.8.

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::{Class, Origin, Trees};
use crate::commit::{InFlight, disk_first, finishes};
use crate::guard::RemoteRemoval;
use crate::ids::IdGen;
use crate::ignore::is_rules_file;

use crate::names::name_key;
use crate::state::{EffectsChanges, EffectsState, StateError};
use crate::tracked::{Marked, TrackedMap};
use crate::tree::{Content, FileIdentity, Kind, LocalNode, NodeId, Shape, same, same_content};

/// A directory on the way to an entry, with the identity L holds for it. The driver resolves a
/// location one directory at a time and fails the effect when an identity differs (section 5.2).
#[derive(Clone, Debug, PartialEq)]
pub struct Dir {
    pub name: String,
    pub identity: FileIdentity,
}

/// Where the directories of a location start.
#[derive(Clone, Debug, PartialEq)]
pub enum Base {
    Root,
    /// The parked directory `.koidrive/parked/<node>`, which must have `identity`.
    Parked {
        node: NodeId,
        identity: FileIdentity,
    },
}

/// An entry's place on disk.
#[derive(Clone, Debug, PartialEq)]
pub enum Location {
    /// `.koidrive/parked/<node_id>` itself.
    Parked(NodeId),
    /// `name` inside the last of `dirs`, which start at `base`.
    At {
        base: Base,
        dirs: Vec<Dir>,
        name: String,
    },
}

/// A file an effect displaces and what it must still be: the driver quarantines it, compares it
/// with this, and moves it to the local trash only on a match.
#[derive(Clone, Debug, PartialEq)]
pub struct Displaced {
    pub at: Location,
    pub identity: FileIdentity,
    pub content: Content,
}

/// One disk change of section 2.5. Every name change is a `mkdir` or rename that never replaces an
/// existing entry.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    CreateDir {
        at: Location,
    },
    /// Downloads `content` into staging and puts it at `at`. With `replaces`, that file goes
    /// to quarantine first; it sits elsewhere when the node also moves.
    WriteFile {
        at: Location,
        content: Content,
        replaces: Option<Displaced>,
    },
    /// Renames the entry with `identity`, also into and out of the parked directory.
    Move {
        from: Location,
        identity: FileIdentity,
        to: Location,
    },
    RemoveFile {
        displaced: Displaced,
    },
    /// Removes a directory, which must be empty once quarantined.
    RemoveDir {
        at: Location,
        identity: FileIdentity,
    },
}

/// An effect as it is persisted before the driver receives it.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectRecord {
    pub effect_id: String,
    pub node: NodeId,
    /// The value the driver writes to the root marker before it touches the disk: the effects
    /// cleared so far plus the effects in flight (section 2.8).
    pub counter: u64,
    pub effect: Effect,
    /// L's shape for the node once the effect is done, or ⊥ for a removal.
    pub after: Option<Shape>,
}

impl EffectRecord {
    pub fn staging(&self) -> String {
        format!(".koidrive/staging/{}", self.effect_id)
    }

    pub fn quarantine(&self) -> String {
        format!(".koidrive/quarantine/{}", self.effect_id)
    }

    /// Whether L takes the outcome of this effect as L is now, by the checks [`Effects::plan`] makes
    /// before it plans one. The engine plans an effect for L as it is then, and a later observation
    /// may have given the effect's name to another node, taken a folder on its way out of L, moved
    /// such a folder under the node, put an entry into a folder it removes, or put the node it
    /// creates into L. A commit's rebase or result may also have given the entry that a move or a
    /// replacement takes to another node of L. L also keeps the position of a node that a commit in
    /// flight moves or creates, while that commit is in flight, and of a node the user moved, as
    /// when a scan sees the user's move after the effect was planned. The planner plans none of
    /// these either, so an effect that fails for one of them and is unblocked is planned again only
    /// once L or R has changed.
    pub fn applies(&self, trees: &Trees, in_flight: Option<&InFlight>) -> bool {
        View::new(trees).applies(self, in_flight)
    }

    fn parks(&self) -> bool {
        matches!(
            self.effect,
            Effect::Move {
                to: Location::Parked(_),
                ..
            }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectError {
    /// The ID is not an effect in flight.
    NotInFlight(String),
    /// A created or written entry was reported done without its file identity.
    MissingIdentity,
    /// L does not take the outcome of this effect in flight ([`EffectRecord::applies`]).
    DoesNotApply(String),
}

/// The disk side of a space: the effects in flight, the effects cleared so far, and the effect that
/// failed last for each blocked node. An effect that failed is never emitted again unchanged, so it
/// is not retried blindly; a change to anything it was computed from, such as the identity of a
/// directory on its path, gives the node a new effect.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effects {
    in_flight: Marked<Vec<EffectRecord>>,
    cleared: Marked<u64>,
    failed: TrackedMap<NodeId, Effect>,
}

/// The most one batch of effects holds: [`Effects::plan`] stops adding effects at `effects`, and adds a write
/// only while the contents of the batch's writes stay within `bytes`, except for its first effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchLimit {
    pub effects: usize,
    pub bytes: u64,
}

impl BatchLimit {
    pub const ONE: BatchLimit = BatchLimit {
        effects: 1,
        bytes: 0,
    };
}

impl Effects {
    /// What the store keeps of the disk side.
    pub fn state(&self) -> EffectsState {
        EffectsState {
            in_flight: (*self.in_flight).clone(),
            cleared: *self.cleared,
            failed: (*self.failed).clone(),
        }
    }

    /// What changed in [`Effects::state`] since the last call, for the store to write.
    pub fn take_changes(&mut self) -> EffectsChanges {
        let Self {
            in_flight,
            cleared,
            failed,
        } = self;
        EffectsChanges {
            in_flight: in_flight.take_changed().cloned(),
            cleared: cleared.take_changed().copied(),
            failed: failed.take_changes(),
        }
    }

    /// The disk side the store kept. Refuses an effect in flight whose counter is not the effects cleared plus the
    /// effects in flight.
    pub fn from_state(state: EffectsState) -> Result<Self, StateError> {
        let upper = state.cleared.checked_add(state.in_flight.len() as u64);
        if let Some(record) = (state.in_flight.iter()).find(|record| Some(record.counter) != upper)
        {
            return Err(StateError::EffectCounter {
                cleared: state.cleared,
                counter: record.counter,
            });
        }
        let EffectsState {
            in_flight,
            cleared,
            failed,
        } = state;
        Ok(Self {
            in_flight: in_flight.into(),
            cleared: cleared.into(),
            failed: failed.into(),
        })
    }

    /// The effects in flight, in the order they were planned.
    pub fn in_flight(&self) -> &[EffectRecord] {
        &self.in_flight
    }

    /// The effect in flight with `effect_id`.
    pub fn flight(&self, effect_id: &str) -> Option<&EffectRecord> {
        (self.in_flight.iter()).find(|record| record.effect_id == effect_id)
    }

    /// The effect counter *e* of section 2.8.
    pub fn cleared(&self) -> u64 {
        *self.cleared
    }

    /// Continues from the root marker's count `marker` when the store is older than the disk
    /// (sections 2.8 and 6.4). The disk carried out every effect it counts, the one in flight
    /// included, so none is recovered, and what they did is read from the disk by the next scan.
    pub fn catch_up(&mut self, marker: u64) {
        self.in_flight.set(Vec::new());
        self.failed.clear();
        self.cleared.set(marker);
    }

    /// The nodes whose last effect failed, with that effect. Each stays blocked until the effect
    /// planned for it differs, or until [`Effects::unblock`].
    pub fn blocked(&self) -> &BTreeMap<NodeId, Effect> {
        &self.failed
    }

    /// Lets the planner emit the failed effect of `node` again, for a caller that knows the
    /// cause is gone. Returns false when the node was not blocked.
    pub fn unblock(&mut self, node: &str) -> bool {
        self.failed.remove(node).is_some()
    }

    /// Emits the next batch of effects, unless effects are in flight, and returns what it emitted. A dirty
    /// node whose only change is remote, or that is parked, gets the effect that makes L equal to R when it applies
    /// to L as it is: its parent is a directory there, no other entry there holds its name key, and a move does not
    /// go into the node's own subtree. A directory is removed only once it is empty. When no such effect applies, a
    /// node that blocks one is parked, if it is a candidate itself or its S update waits ([`Trees::waiting`]), since
    /// the commit layer does not plan it either: the entry holding a name, or a directory that would move into its
    /// own subtree. A blocker that already sits where R has it is not parked, since its own effect would move it
    /// straight back and take the name again. A node whose park failed gets no effect until L changes: the parked
    /// directory always has room, so the park failed on the entry itself, and the node's own effect would fail on
    /// it too, taking turns with the park for as long as L stays as it is. A rename that changes only case or
    /// normalization goes through the parked directory too.
    ///
    /// A batch holds effects that each apply to L as it is and that leave each other applying: no two give a name
    /// key in one folder to their nodes, and none changes a folder on another's path, since a batch never holds a
    /// folder's move. A park, a move out of the parked directory, a folder's move and an effect on a rules file are
    /// planned alone. So applying the batch in any order, or only part of it, gives what applying it one effect at a
    /// time does. Every effect of a batch carries the same counter, the effects cleared plus the batch's size, which
    /// the driver writes to the root marker before it touches the disk (section 2.8).
    ///
    /// A parked node with a local change, such as an edit observed in the parked directory, is
    /// only moved out: to its R position, or to the position L keeps for it when R has none. Its
    /// content is never replaced or removed, so the change is planned like any other local one.
    ///
    /// An excluded node ([`Trees::excluded`]) gets no effect and is never parked, and neither is a
    /// node whose own place an excluded node holds. A parked node that the rules now leave out is
    /// only moved back to the position L keeps for it, and nothing is planned while the rules are
    /// unknown ([`Trees::rules_known`]), or for a node below a folder whose rules are unknown
    /// ([`Trees::held`]). A `.koiignore` file that R
    /// holds and the disk does not yet is written before any other effect below its folder
    /// (section 5.10). The driver reads the rules of a rules file it wrote and passes them to
    /// [`Trees::set_rules`] before it asks for the next effect, so the rules are in force before
    /// the nodes they cover arrive.
    pub fn plan(
        &mut self,
        trees: &Trees,
        in_flight: Option<&InFlight>,
        ids: &mut IdGen,
        now_ms: u64,
        limit: BatchLimit,
    ) -> &[EffectRecord] {
        if !self.in_flight.is_empty() || !trees.rules_known() || limit.effects == 0 {
            return &[];
        }
        let none = BTreeSet::new();
        let finishing: BTreeSet<&NodeId> = (finishes(trees, &none, None))
            .filter_map(|(id, .., free)| free.then_some(id))
            .collect();
        self.failed.retain(|id, _| {
            candidate(trees, id) || trees.waiting().contains_key(id) || finishing.contains(id)
        });
        let chosen = self.choose(trees, in_flight, limit);
        let counter = *self.cleared + chosen.len() as u64;
        for (node, effect, after) in chosen {
            let target = match &effect {
                Effect::CreateDir { at }
                | Effect::WriteFile { at, .. }
                | Effect::Move { to: at, .. } => Some(at),
                Effect::RemoveFile { .. } | Effect::RemoveDir { .. } => None,
            };
            let after = match (after, target) {
                (Some(after), Some(Location::At { name, .. })) => Some(Shape {
                    name: name.clone(),
                    ..after
                }),
                (after, _) => after,
            };
            self.failed.remove(&node);
            self.in_flight.get_mut().push(EffectRecord {
                effect_id: ids.uuid_v7(now_ms),
                node,
                counter,
                effect,
                after,
            });
        }
        self.in_flight.as_slice()
    }

    fn choose(
        &self,
        trees: &Trees,
        in_flight: Option<&InFlight>,
        limit: BatchLimit,
    ) -> Vec<(NodeId, Effect, Option<Shape>)> {
        let view = View::new(trees);
        let writes_rules = |id: &str| {
            let remote = trees.remote().live(id)?;
            (remote.kind == Kind::File && is_rules_file(&remote.name))
                .then_some(remote.parent.as_str())
        };
        let gates: BTreeSet<&str> = (trees.remote_rules_files().iter())
            .filter(|id| trees.dirty().contains(*id))
            .filter_map(|id| writes_rules(id).filter(|_| candidate(trees, id)))
            .collect();
        let gated = |id: &str| {
            let parent = |id: &str| {
                let remote = trees.remote().live(id);
                let shape = remote.or(trees.local().get(id).map(|node| &node.shape))?;
                Some(shape.parent.as_str())
            };
            let mut above = parent(id);
            if writes_rules(id).is_some() {
                above = above.and_then(parent);
            }
            for _ in 0..=trees.remote().len() + trees.local().len() {
                match above {
                    Some(dir) if gates.contains(dir) => return true,
                    Some(dir) => above = parent(dir),
                    None => return false,
                }
            }
            false
        };
        let park_failed = |id: &str| {
            let failed = self.failed.get(id);
            matches!(
                failed,
                Some(Effect::Move {
                    to: Location::Parked(_),
                    ..
                })
            ) && view.park(id).map(|(park, _)| park).as_ref() == failed
        };
        let mut blockers = BTreeSet::new();
        let mut chosen = Vec::new();
        let mut claimed = BTreeSet::new();
        let mut bytes = 0;
        for id in trees.dirty() {
            if chosen.len() >= limit.effects {
                break;
            }
            if !candidate(trees, id) || trees.held(id) || (!gates.is_empty() && gated(id)) {
                continue;
            }
            match view.step(id) {
                Step::Apply(effect, _)
                    if self.failed.get(id) == Some(&effect) || park_failed(id) => {}
                Step::Apply(_, Some(after)) if view.holds_position(id, &after, in_flight) => {}
                Step::Apply(effect, after) if !batchable(trees, id, &effect) => {
                    if chosen.is_empty() {
                        return vec![(id.clone(), effect, after)];
                    }
                }
                Step::Apply(effect, after) => {
                    let claim =
                        (after.as_ref()).map(|after| (after.parent.clone(), name_key(&after.name)));
                    let size = match &effect {
                        Effect::WriteFile { content, .. } => content.size,
                        _ => 0,
                    };
                    if !chosen.is_empty() && bytes + size > limit.bytes {
                        break;
                    }
                    if claim.as_ref().is_none_or(|claim| !claimed.contains(claim)) {
                        bytes += size;
                        claimed.extend(claim);
                        chosen.push((id.clone(), effect, after));
                    }
                }
                Step::Blocked(Some(blocker)) => {
                    blockers.insert(blocker);
                }
                Step::Blocked(None) | Step::Skip => {}
            }
        }
        if !chosen.is_empty() {
            return chosen;
        }
        let none = BTreeSet::new();
        let finish = (finishes(trees, &none, None))
            .filter(|(id, ..)| gates.is_empty() || !gated(id))
            .find_map(|(id, parent, name, free)| {
                let local = (trees.local().get(id)).filter(|_| free && !disk_first(trees, id))?;
                let after = Shape {
                    parent: parent.clone(),
                    name: name.to_owned(),
                    ..local.shape.clone()
                };
                let effect = Effect::Move {
                    from: view.location(id)?,
                    identity: local.identity,
                    to: view.location_in(parent, name)?,
                };
                (self.failed.get(id) != Some(&effect)
                    && !view.holds_position(id, &after, in_flight))
                .then(|| (id.clone(), effect, Some(after)))
            });
        if let Some(finish) = finish {
            return vec![finish];
        }
        (blockers.into_iter())
            .find_map(|id| {
                if !view.parkable(&id) {
                    return None;
                }
                let (effect, after) = view.park(&id)?;
                (self.failed.get(&id) != Some(&effect)).then_some((id, effect, after))
            })
            .into_iter()
            .collect()
    }

    /// The driver did the effect. L takes the node's new shape, with `identity` for an entry the
    /// effect created. When L matched S, or the shape S's waiting update holds, the effect gave L
    /// the shape R had when it was planned, so S takes that shape too. When that would not leave S
    /// a valid tree yet, as when S still holds a child the user moved out of a removed folder, the
    /// update waits ([`Trees::waiting`]) instead of being lost.
    /// A move changes only the position, so for a move L only has to match that shape's position,
    /// and L and S each keep their content, which can be newer than the content the move was
    /// planned with. An edit the user made while the move was in flight stays the user's.
    /// A change R made while the effect was in flight then stays a remote change and is never
    /// taken for the user's: a file R restored during its removal is downloaded again, and a file
    /// R moved back during its move is moved back. A finished removal leaves S without the node
    /// even when L no longer matched S: the effect took the entry off the disk, so a change of the
    /// user's to it is gone with it, and a node R brings back later is downloaded, never deleted
    /// as the user's. For any other effect, S stays as it is when L no longer matched it: L then
    /// holds a change of the user's, such as the planned bytes that recovery after a crash reports
    /// as a write's outcome, and that change stays the user's.
    /// An outcome L does not take ([`EffectRecord::applies`]) is refused, and the effect stays
    /// in flight.
    /// A removal of a node R no longer holds applied another device's deletion, and is returned
    /// for the mass-delete guard to count ([`Commits::count_remote`]).
    ///
    /// [`Commits::count_remote`]: crate::commit::Commits::count_remote
    pub fn done(
        &mut self,
        trees: &mut Trees,
        in_flight: Option<&InFlight>,
        effect_id: &str,
        identity: Option<FileIdentity>,
    ) -> Result<Option<RemoteRemoval>, EffectError> {
        let flight = self.flight(effect_id);
        if flight.is_some_and(|record| !record.applies(trees, in_flight)) {
            return Err(EffectError::DoesNotApply(effect_id.to_owned()));
        }
        let identity = match flight.map(|record| &record.effect) {
            Some(Effect::CreateDir { .. } | Effect::WriteFile { .. }) => {
                identity.ok_or(EffectError::MissingIdentity)?
            }
            Some(
                Effect::Move { identity, .. }
                | Effect::RemoveDir { identity, .. }
                | Effect::RemoveFile {
                    displaced: Displaced { identity, .. },
                },
            ) => *identity,
            None => return Err(EffectError::NotInFlight(effect_id.to_owned())),
        };
        let record = self.take(effect_id)?;
        let removal =
            (record.after.is_none() && trees.remote().live(&record.node).is_none()).then(|| {
                RemoteRemoval {
                    within: within(trees, &record.node),
                    files: u64::from(matches!(record.effect, Effect::RemoveFile { .. })),
                }
            });
        let parked = record.parks();
        let base = trees.acknowledged(&record.node);
        let local = trees.local().live(&record.node);
        let unchanged = match (&record.effect, local, base) {
            (Effect::Move { .. }, Some(local), Some(base)) => {
                local.kind == base.kind && local.same_position(base)
            }
            _ => same(local, base),
        };
        let finished = finishing_move(trees, &record);
        let to_pending = matches!(record.effect, Effect::Move { .. })
            && (record.after.as_ref()).is_some_and(|after| {
                (trees.pending_finishes().get(&record.node)).is_some_and(|finish| {
                    finish.parent == after.parent && finish.pending == after.name
                })
            });
        if (unchanged && !finished) || record.after.is_none() {
            let synced = match (&record.effect, &record.after, base) {
                (Effect::Move { .. }, Some(after), Some(base)) => Some(Shape {
                    parent: after.parent.clone(),
                    name: after.name.clone(),
                    ..base.clone()
                }),
                _ => record.after.clone(),
            };
            let origin =
                (trees.waiting().get(&record.node)).map_or(Origin::Other, |waiting| waiting.origin);
            if !trees.set_synced_if_valid(record.node.clone(), synced.clone()) {
                trees.wait(record.node.clone(), synced, origin);
            }
        }
        let content = (trees.local().get(&record.node)).map(|node| node.shape.content.clone());
        match record.after {
            None => trees.remove_local(&record.node),
            Some(shape) => trees.set_local(
                record.node.clone(),
                LocalNode {
                    shape: match (&record.effect, content) {
                        (Effect::Move { .. }, Some(content)) => Shape { content, ..shape },
                        _ => shape,
                    },
                    identity,
                    parked,
                },
            ),
        }
        if finished {
            trees.start_finish(&record.node);
        }
        if to_pending {
            trees.undo_disk_first(&record.node);
        }
        self.failed.remove(&record.node);
        self.cleared.set(*self.cleared + 1);
        Ok(removal)
    }

    /// Whether L confirms what recovery reports after a crash: that the effect in flight is done,
    /// with `identity` for the entry it created (section 5.3). Recovery judges a target by the name
    /// and the content it finds there, so an entry that L holds as another node is that node's. A
    /// removal is done only once its entry is in the local trash under the effect's ID, which the
    /// effect put there after it matched the entry against what it displaced, so L's position for
    /// the node does not refute it.
    pub fn confirms(
        &self,
        trees: &Trees,
        effect_id: &str,
        identity: Option<&FileIdentity>,
    ) -> bool {
        let Some(record) = self.flight(effect_id) else {
            return false;
        };
        !(identity.into_iter())
            .flat_map(|identity| trees.locals_with_identity(identity))
            .any(|id| *id != record.node)
    }

    /// The driver did not do the effect. Its observations of the disk go to L separately; the node
    /// is blocked until the effect planned for it differs from this one. A move that finishes a
    /// pending rename, and that L already holds because the user made it, starts the finish as if
    /// it were done.
    pub fn failed(&mut self, trees: &mut Trees, effect_id: &str) -> Result<(), EffectError> {
        let record = self.take(effect_id)?;
        self.cleared.set(*self.cleared + 1);
        let made = (record.after.as_ref()).is_some_and(|after| {
            (trees.local().live(&record.node)).is_some_and(|local| local.same_position(after))
        });
        if made && finishing_move(trees, &record) {
            trees.start_finish(&record.node);
        } else {
            self.failed.insert(record.node, record.effect);
        }
        Ok(())
    }

    /// The effect in flight sent the entry it replaced to the local trash, and its target does not
    /// hold R's content: the user changed or removed the written file, or took its name before it
    /// was placed (section 5.3). S's content is gone from the disk, so the node leaves S and L: R's
    /// content is downloaded again and what the user left is kept beside it, also when R returns to
    /// the bytes S held.
    pub fn overtaken(&mut self, trees: &mut Trees, effect_id: &str) -> Result<(), EffectError> {
        let record = self.take(effect_id)?;
        trees.remove_local(&record.node);
        trees.set_synced(record.node, None);
        self.cleared.set(*self.cleared + 1);
        Ok(())
    }

    fn take(&mut self, effect_id: &str) -> Result<EffectRecord, EffectError> {
        let position = (self.in_flight.iter())
            .position(|record| record.effect_id == effect_id)
            .ok_or_else(|| EffectError::NotInFlight(effect_id.to_owned()))?;
        Ok(self.in_flight.get_mut().remove(position))
    }
}

/// Whether `effect` of node `id` may share a batch with other effects ([`Effects::plan`]): it names no entry
/// through the parked directory, moves no folder and touches no rules file, so it changes no path another effect
/// of the batch takes and no rule another effect was planned under.
fn batchable(trees: &Trees, id: &str, effect: &Effect) -> bool {
    let plain = |at: &Location| matches!(at, Location::At { base: Base::Root, name, .. } if !is_rules_file(name));
    match effect {
        Effect::CreateDir { at } | Effect::RemoveDir { at, .. } => plain(at),
        Effect::WriteFile { at, replaces, .. } => {
            plain(at)
                && replaces
                    .as_ref()
                    .is_none_or(|displaced| plain(&displaced.at))
        }
        Effect::RemoveFile { displaced } => plain(&displaced.at),
        Effect::Move { from, to, .. } => {
            plain(from)
                && plain(to)
                && (trees.local().get(id)).is_some_and(|node| node.shape.kind == Kind::File)
        }
    }
}

fn finishing_move(trees: &Trees, record: &EffectRecord) -> bool {
    let base = trees.acknowledged(&record.node);
    matches!(record.effect, Effect::Move { .. })
        && (record.after.as_ref()).is_some_and(|after| {
            match trees.pending_finishes().get(&record.node) {
                Some(finish) => finish.parent == after.parent && finish.name == after.name,
                None => (trees.remote().get(&record.node)).is_some_and(|remote| {
                    !remote.trashed
                        && remote.shape.parent == after.parent
                        && remote.pending_for.as_ref() == Some(&after.name)
                        && base.is_some_and(|base| base.same_position(&remote.shape))
                }),
            }
        })
}

/// The deletions a removal of `node` can belong to: the node and each folder above it in L that R
/// no longer holds, innermost first.
fn within(trees: &Trees, node: &NodeId) -> Vec<NodeId> {
    let mut within = vec![node.clone()];
    for _ in 0..trees.local().len() {
        match trees.local().get(within.last().expect("the node")) {
            Some(local)
                if trees.local().get(&local.shape.parent).is_some()
                    && trees.remote().live(&local.shape.parent).is_none() =>
            {
                within.push(local.shape.parent.clone());
            }
            _ => break,
        }
    }
    within
}

pub(crate) fn held_by_excluded(trees: &Trees, id: &str, target: &Shape) -> bool {
    View::new(trees).held_by_excluded(id, target)
}

pub(crate) fn kept_out(trees: &Trees, id: &str) -> bool {
    let view = View::new(trees);
    trees.local().get(id).is_some_and(|node| node.parked)
        && candidate(trees, id)
        && !trees.held(id)
        && !trees.ignored(id)
        && matches!(view.step(id), Step::Blocked(Some(holder)) if !view.parkable(&holder))
}

/// A node the planner considers: one whose only change is remote, one that is parked, one whose
/// local position is reverted, or one whose S update waits and that R no longer has. A reverted or
/// waiting node that R no longer has is removed only when it holds no unuploaded edit; otherwise
/// the commit layer makes it a new node, so the edit is uploaded. A node whose waiting update
/// removes it from S is one only when it is reverted and L no longer holds it: the keep rule could
/// not clear it from S, and what S holds under it may wait for it to be back on disk. Any other
/// such node is not one until the update is made: a node L holds is then one S does not, which is
/// created again ([`Trees::forget_lost`]), and one L lacks is downloaded again. An excluded node
/// is never one.
fn candidate(trees: &Trees, id: &str) -> bool {
    let forgets = (trees.waiting().get(id)).is_some_and(|waiting| waiting.shape.is_none())
        && (trees.local().get(id).is_some() || !trees.reverted().contains(id));
    let changed = remote_only(trees, id)
        || trees.local().get(id).is_some_and(|node| node.parked)
        || (trees.reverted().contains(id)
            && (trees.local().get(id).is_some()
                || holds_below(trees, id)
                || arrives_below(trees, id)
                || trees.kept_for_excluded()(id))
            && (trees.remote().live(id).is_some() || !trees.edited(id)))
        || (trees.waiting().contains_key(id)
            && trees.remote().live(id).is_none()
            && !trees.edited(id))
        || trees.is_discarded(id);
    changed && !forgets && !trees.excluded(id) && !trees.reclaimed(id)
}

/// Why a node stays as it is for as long as this device's rules do (section 5.10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stuck {
    /// The local copy of this excluded node holds the place the node goes to, itself or through a
    /// node stuck the same way. No effect moves an excluded node, and the node is not parked.
    HeldBy(NodeId),
    /// The node's S update waits for the name this node keeps in S, a node held by an excluded
    /// one, or for a folder that waits so, or the node cannot be put into its folder in R
    /// because that folder is stuck so.
    Behind(NodeId),
    /// The node is parked, and neither its place in R nor the one L keeps for it is free here.
    Parked,
}

/// The nodes section 5.10 reports as problems: each node the planner considers whose place is held
/// by the local copy of an excluded node, each node whose S update waits behind one of them, and
/// each node that cannot be put into its folder in R because that folder is one of them.
pub fn stuck(
    trees: &Trees,
    effects: &Effects,
    in_flight: Option<&InFlight>,
) -> BTreeMap<NodeId, Stuck> {
    let view = View::new(trees);
    let planned: Vec<&NodeId> = (trees.dirty().iter())
        .filter(|id| candidate(trees, id) && !trees.held(id))
        .collect();
    let unplaced: Vec<(&NodeId, &str)> = (planned.iter())
        .filter(|id| matches!(view.step(id), Step::Blocked(None)))
        .filter_map(|id| Some((*id, trees.remote().live(id)?.parent.as_str())))
        .collect();
    let mut stuck: BTreeMap<NodeId, Stuck> = (planned.iter())
        .filter_map(|id| {
            let by = view.stuck(id, trees.local().len())?;
            Some(((*id).clone(), Stuck::HeldBy(by)))
        })
        .collect();
    let idle = effects.in_flight().is_empty()
        && effects.choose(trees, in_flight, BatchLimit::ONE).is_empty();
    let parked: Vec<(NodeId, Stuck)> = (planned.iter())
        .filter(|_| idle)
        .filter(|id| !stuck.contains_key(**id))
        .filter(|id| trees.local().get(id).is_some_and(|node| node.parked))
        .map(|id| {
            let held_by =
                (trees.remote().live(id)).and_then(|remote| match view.blocked(id, remote) {
                    Some(Step::Blocked(Some(holder))) if trees.excluded(&holder) => Some(holder),
                    _ => None,
                });
            ((*id).clone(), held_by.map_or(Stuck::Parked, Stuck::HeldBy))
        })
        .collect();
    stuck.extend(parked);
    let root = |stuck: &BTreeMap<NodeId, Stuck>, id: &str| match stuck.get(id)? {
        Stuck::HeldBy(_) | Stuck::Parked => Some(id.to_owned()),
        Stuck::Behind(root) => Some(root.clone()),
    };
    loop {
        let mut behind: Vec<(NodeId, NodeId)> = (trees.waiting().iter())
            .filter(|(id, _)| !stuck.contains_key(*id))
            .filter_map(|(id, waiting)| {
                let shape = waiting.shape.as_ref()?;
                let key = name_key(&shape.name);
                let holder = trees.synced_children(&shape.parent).find(|other| {
                    *other != id
                        && (trees.synced().live(other)).is_some_and(|s| name_key(&s.name) == key)
                });
                let root = match holder {
                    Some(holder) => root(&stuck, holder),
                    None if trees.synced().live(&shape.parent).is_none() => {
                        root(&stuck, &shape.parent)
                    }
                    None => None,
                };
                Some((id.clone(), root?))
            })
            .collect();
        behind.extend(
            (unplaced.iter())
                .filter(|(id, _)| !stuck.contains_key(*id))
                .filter_map(|(id, parent)| Some(((*id).clone(), root(&stuck, parent)?))),
        );
        if behind.is_empty() {
            return stuck;
        }
        stuck.extend(
            behind
                .into_iter()
                .map(|(id, root)| (id, Stuck::Behind(root))),
        );
    }
}

fn arrives_below(trees: &Trees, id: &str) -> bool {
    let remote = trees.remote();
    remote.iter().any(|(node, _)| {
        if !remote_only(trees, node) || trees.excluded(node) {
            return false;
        }
        let mut up = remote.live(node);
        for _ in 0..=remote.len() {
            match up {
                Some(shape) if shape.parent == id => return true,
                Some(shape) => up = remote.live(&shape.parent),
                None => return false,
            }
        }
        false
    })
}

/// Whether L holds a node whose way to the root passes `id`. Each node on that way goes to its R
/// position, or to its local one when R no longer has it, as a parked edit that is moved back, or
/// when it is parked and ignored, as [`View::step`] moves it back.
fn holds_below(trees: &Trees, id: &str) -> bool {
    let (local, remote) = (trees.local(), trees.remote());
    let parent = |node: &str| {
        let home = local
            .get(node)
            .filter(|held| {
                held.parked
                    && (trees.ignored(node)
                        || remote
                            .live(node)
                            .is_some_and(|shape| View::new(trees).goes_home(node, shape)))
            })
            .map(|held| &held.shape);
        (home
            .or_else(|| remote.live(node))
            .or_else(|| local.live(node)))
        .map(|shape| shape.parent.as_str())
    };
    local.iter().any(|(node, _)| {
        let mut up = parent(node);
        for _ in 0..=remote.len() + local.len() {
            match up {
                Some(parent) if parent == id => return true,
                Some(next) => up = parent(next),
                None => return false,
            }
        }
        false
    })
}

/// Whether the node's only change is remote. While its S update waits, the waiting shape stands
/// for S: L still holds the shape the server acknowledged, and only R differs from it. The update
/// may never fit S while L holds that shape, as when its name key is taken in S, so the effects
/// take R's shape and the update stops waiting once L leaves it.
fn remote_only(trees: &Trees, id: &str) -> bool {
    trees.acknowledged_class(id) == Class::Remote
}

/// Where the disk holds the entry of the node L holds as `id`: in the parked directory, below a
/// parked folder, or below the root.
pub fn location_of(trees: &Trees, id: &str) -> Option<Location> {
    View::new(trees).location(id)
}

pub(crate) fn unparks_to(trees: &Trees, id: &str, target: &Shape) -> bool {
    let view = View::new(trees);
    view.blocked(id, target).is_none()
        && !forked_aside(trees, id, target)
        && (view.location_in(&target.parent, &target.name)).is_some()
}

fn forked_aside(trees: &Trees, id: &str, shape: &Shape) -> bool {
    trees.remote().live(id).is_none()
        && trees.edited(id)
        && (trees.remote().iter()).any(|(other, node)| {
            other != id
                && !node.trashed
                && node.shape.parent == shape.parent
                && name_key(&node.shape.name) == name_key(&shape.name)
                && !trees.excluded(other)
        })
}

#[allow(clippy::large_enum_variant)]
enum Step {
    Apply(Effect, Option<Shape>),
    /// Waits, for the given node to move away when there is one.
    Blocked(Option<NodeId>),
    Skip,
}

/// L as the disk holds it: parked nodes sit in the parked directory and hold no name.
struct View<'a> {
    trees: &'a Trees,
}

impl<'a> View<'a> {
    fn new(trees: &'a Trees) -> Self {
        Self { trees }
    }

    fn root(&self) -> &'a str {
        &self.trees.local().root
    }

    fn step(&self, id: &str) -> Step {
        let local = self.trees.local().get(id);
        let remote = self.trees.remote().live(id);
        if let Some(local) = local.filter(|local| local.parked && self.trees.ignored(id)) {
            let away = remote.filter(|remote| {
                self.held_by_excluded(id, &local.shape)
                    && !(self.trees.edited(id) && self.trees.ignored_in_remote(id))
                    && self.blocked(id, remote).is_none()
            });
            return self.unpark(id, local, away.unwrap_or(&local.shape));
        }
        let (local, remote) = match (local, remote) {
            (None, None) => return Step::Skip,
            (Some(local), None)
                if local.parked
                    && !remote_only(self.trees, id)
                    && !self.trees.is_discarded(id)
                    && (!(self.trees.reverted().contains(id)
                        || self.trees.waiting().contains_key(id))
                        || self.trees.edited(id)) =>
            {
                if forked_aside(self.trees, id, &local.shape) {
                    return Step::Blocked(None);
                }
                return self.unpark(id, local, &local.shape);
            }
            (Some(local), None) => return self.remove(id, local),
            (None, Some(remote)) => {
                if let Some(blocked) = self.blocked(id, remote) {
                    return blocked;
                }
                let Some(at) = self.location_in(&remote.parent, &remote.name) else {
                    return Step::Skip;
                };
                let effect = match (&remote.kind, &remote.content) {
                    (Kind::Dir, _) => Effect::CreateDir { at },
                    (Kind::File, Some(content)) => Effect::WriteFile {
                        at,
                        content: content.clone(),
                        replaces: None,
                    },
                    (Kind::File, None) => return Step::Skip,
                };
                return Step::Apply(effect, Some(remote.clone()));
            }
            (Some(local), Some(remote)) => (local, remote),
        };
        if local.parked && self.goes_home(id, remote) {
            return self.unpark(id, local, &local.shape);
        }
        if local.parked && !remote_only(self.trees, id) {
            return self.unpark(id, local, remote);
        }
        if local.shape.kind != remote.kind {
            return Step::Skip;
        }
        let rewrite = match (&local.shape.content, &remote.content) {
            (Some(old), Some(new)) => !same_content(old, new) && remote_only(self.trees, id),
            _ => false,
        };
        let moved = local.parked || !local.shape.same_position(remote);
        let Some(here) = self.location(id) else {
            return Step::Skip;
        };
        if !moved {
            if !rewrite {
                return Step::Skip;
            }
            let displaced = self.displaced(here.clone(), local);
            return match (displaced, &remote.content) {
                (Some(displaced), Some(content)) => Step::Apply(
                    Effect::WriteFile {
                        at: here,
                        content: content.clone(),
                        replaces: Some(displaced),
                    },
                    Some(remote.clone()),
                ),
                _ => Step::Skip,
            };
        }
        if self.trees.edited(id) && self.trees.ignored_in_remote(id) {
            return Step::Skip;
        }
        if !local.parked
            && local.shape.parent == remote.parent
            && name_key(&local.shape.name) == name_key(&remote.name)
        {
            return match self.park(id) {
                Some((effect, after)) => Step::Apply(effect, after),
                None => Step::Skip,
            };
        }
        if let Some(blocked) = self.blocked(id, remote) {
            return blocked;
        }
        let Some(to) = self.location_in(&remote.parent, &remote.name) else {
            return Step::Skip;
        };
        if rewrite {
            let displaced = self.displaced(here, local);
            return match (displaced, &remote.content) {
                (Some(displaced), Some(content)) => Step::Apply(
                    Effect::WriteFile {
                        at: to,
                        content: content.clone(),
                        replaces: Some(displaced),
                    },
                    Some(remote.clone()),
                ),
                _ => Step::Skip,
            };
        }
        let after = Shape {
            parent: remote.parent.clone(),
            name: remote.name.clone(),
            ..local.shape.clone()
        };
        Step::Apply(
            Effect::Move {
                from: here,
                identity: local.identity,
                to,
            },
            Some(after),
        )
    }

    /// Whether L takes `record`'s outcome: a removal once the node holds no entry, a park always,
    /// a create while L does not hold the node, and anything else while its folder is one
    /// [`View::step`] could place the node in and no other node holds its exact name. L tells
    /// names apart exactly, so a name another node holds in another case does not stop it.
    fn applies(&self, record: &EffectRecord, in_flight: Option<&InFlight>) -> bool {
        let Some(after) = &record.after else {
            return self.trees.local_children(&record.node).next().is_none();
        };
        let kept = match &record.effect {
            Effect::Move { identity, .. } => Some(identity),
            Effect::WriteFile {
                replaces: Some(displaced),
                ..
            } => Some(&displaced.identity),
            _ => None,
        };
        let held_by_node = |identity: &FileIdentity| {
            (self.trees.local().get(&record.node)).is_some_and(|node| node.identity == *identity)
        };
        if kept.is_some_and(|identity| {
            !held_by_node(identity) && self.trees.local_with_identity(identity).is_some()
        }) {
            return false;
        }
        if record.parks() {
            return true;
        }
        let creates = matches!(
            record.effect,
            Effect::CreateDir { .. } | Effect::WriteFile { replaces: None, .. }
        );
        if creates && self.trees.local().get(&record.node).is_some() {
            return false;
        }
        if self.holds_position(&record.node, after, in_flight) {
            return false;
        }
        let local = self.trees.local();
        self.blocked_in(&record.node, &after.parent).is_none()
            && self.location_in(&after.parent, &after.name).is_some()
            && !(self.trees.local_holders(&after.parent, &after.name)).any(|other| {
                *other != record.node
                    && local
                        .get(other)
                        .is_some_and(|node| node.shape.name == after.name)
            })
    }

    fn goes_home(&self, id: &str, remote: &Shape) -> bool {
        self.trees.edited(id) && self.trees.ignored_in_remote(id)
            || self.held_by_excluded(id, remote)
    }

    fn held_by_excluded(&self, id: &str, target: &Shape) -> bool {
        matches!(
            self.blocked(id, target),
            Some(Step::Blocked(Some(holder))) if self.trees.excluded(&holder)
        )
    }

    /// Moves a parked node to `target`'s position with its content as L has it.
    fn unpark(&self, id: &str, local: &LocalNode, target: &Shape) -> Step {
        if let Some(blocked) = self.blocked(id, target) {
            return blocked;
        }
        let Some(to) = self.location_in(&target.parent, &target.name) else {
            return Step::Skip;
        };
        let after = Shape {
            parent: target.parent.clone(),
            name: target.name.clone(),
            ..local.shape.clone()
        };
        Step::Apply(
            Effect::Move {
                from: Location::Parked(self.trees.parked_name(id).to_owned()),
                identity: local.identity,
                to,
            },
            Some(after),
        )
    }

    fn remove(&self, id: &str, local: &LocalNode) -> Step {
        if self.trees.local_children(id).next().is_some() {
            return Step::Blocked(None);
        }
        let Some(at) = self.location(id) else {
            return Step::Skip;
        };
        let effect = match local.shape.kind {
            Kind::Dir => Effect::RemoveDir {
                at,
                identity: local.identity,
            },
            Kind::File => match self.displaced(at, local) {
                Some(displaced) => Effect::RemoveFile { displaced },
                None => return Step::Skip,
            },
        };
        Step::Apply(effect, None)
    }

    /// Why `id` cannot be put at `remote`'s position in L yet, if it cannot.
    fn blocked(&self, id: &str, remote: &Shape) -> Option<Step> {
        if let Some(step) = self.blocked_in(id, &remote.parent) {
            return Some(step);
        }
        self.trees
            .local_holders(&remote.parent, &remote.name)
            .find(|other| *other != id)
            .map(|holder| Step::Blocked(Some(holder.clone())))
    }

    /// Why `id` cannot be put into `parent` in L yet, if it cannot: `parent` is no folder of L,
    /// or it is `id` or inside it.
    fn blocked_in(&self, id: &str, parent: &str) -> Option<Step> {
        if parent == self.root() {
            return None;
        }
        match self.trees.local().get(parent) {
            Some(node) if node.shape.kind == Kind::Dir => {}
            _ => return Some(Step::Blocked(None)),
        }
        if self.inside(parent, id) {
            return Some(Step::Blocked(None));
        }
        None
    }

    /// The excluded node that keeps `id` from its place, if one does: the name it moves to is held
    /// by an excluded node, whose local copy no effect moves, or by a node that is stuck the same
    /// way. Parking `id` would hide it for as long as the rules stay as they are.
    fn stuck(&self, id: &str, depth: usize) -> Option<NodeId> {
        match self.step(id) {
            Step::Blocked(Some(holder)) if holder != id => {
                if self.trees.excluded(&holder) {
                    return Some(holder);
                }
                self.stuck(&holder, depth.checked_sub(1)?)
            }
            _ => None,
        }
    }

    /// True when `ancestor` is `id` or holds it on disk.
    fn inside(&self, id: &str, ancestor: &str) -> bool {
        let mut current = id;
        for _ in 0..=self.trees.local().len() {
            if current == ancestor {
                return true;
            }
            match self.trees.local().get(current) {
                Some(node) if !node.parked && node.shape.parent != self.root() => {
                    current = &node.shape.parent;
                }
                _ => return false,
            }
        }
        false
    }

    fn holds_position(&self, id: &str, after: &Shape, in_flight: Option<&InFlight>) -> bool {
        let trees = self.trees;
        let Some(local) = trees.local().get(id) else {
            return false;
        };
        if local.shape.same_position(after) {
            return false;
        }
        in_flight.is_some_and(|flight| flight.positions(id))
            || (!local.parked
                && !trees.reverted().contains(id)
                && (trees.acknowledged(id)).is_some_and(|base| !local.shape.same_position(base)))
    }

    fn parkable(&self, id: &str) -> bool {
        let trees = self.trees;
        (candidate(trees, id) || trees.waiting().contains_key(id) || trees.reclaimed(id))
            && !trees.excluded(id)
            && !trees.held(id)
            && trees.local().get(id).is_some_and(|node| {
                !node.parked
                    && !(trees.remote().live(id))
                        .is_some_and(|remote| node.shape.same_position(remote))
            })
            && self.stuck(id, trees.local().len()).is_none()
            && !(trees.remote().live(id)).is_some_and(|remote| self.goes_home(id, remote))
    }

    fn park(&self, id: &str) -> Option<(Effect, Option<Shape>)> {
        let local = self.trees.local().get(id)?;
        let effect = Effect::Move {
            from: self.location(id)?,
            identity: local.identity,
            to: Location::Parked(id.to_owned()),
        };
        Some((effect, Some(local.shape.clone())))
    }

    fn displaced(&self, at: Location, local: &LocalNode) -> Option<Displaced> {
        Some(Displaced {
            at,
            identity: local.identity,
            content: local.shape.content.clone()?,
        })
    }

    fn location(&self, id: &str) -> Option<Location> {
        let node = self.trees.local().get(id)?;
        if node.parked {
            return Some(Location::Parked(self.trees.parked_name(id).to_owned()));
        }
        self.location_in(&node.shape.parent, &node.shape.name)
    }

    fn location_in(&self, parent: &str, name: &str) -> Option<Location> {
        let mut dirs = Vec::new();
        let mut current = parent;
        for _ in 0..=self.trees.local().len() {
            if current == self.root() {
                dirs.reverse();
                return Some(Location::At {
                    base: Base::Root,
                    dirs,
                    name: name.to_owned(),
                });
            }
            let node = self.trees.local().get(current)?;
            if node.parked {
                dirs.reverse();
                return Some(Location::At {
                    base: Base::Parked {
                        node: self.trees.parked_name(current).to_owned(),
                        identity: node.identity,
                    },
                    dirs,
                    name: name.to_owned(),
                });
            }
            dirs.push(Dir {
                name: node.shape.name.clone(),
                identity: node.identity,
            });
            current = &node.shape.parent;
        }
        None
    }
}
