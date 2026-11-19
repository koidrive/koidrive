// SPDX-License-Identifier: Apache-2.0

//! The engine state besides the nodes of R, L and S (design §2.8) and the rows that hold it. This
//! module is the one place that knows both the engine's state types and the tables of migration 3.
//! The restores of the commit side have their rows in [`crate::restore`], and the open questions
//! and the mass-delete guard in [`crate::questions`].

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::classify::{Origin, Waiting};
use koidrive_engine::commit::{Halt, InFlight, Slot};
use koidrive_engine::effects::{Base, Dir, Displaced, Effect, EffectRecord, Location};
use koidrive_engine::question::{RecoveryCause, RecoveryTrigger, Revisions};
use koidrive_engine::restore::SplitRestore;
use koidrive_engine::state::{
    Agreed, CommitsChanges, CommitsState, ContentProvenance, EffectsChanges, EffectsState,
    FinishOrigin, GivenUp, LostPosition, MapChanges, MovedHere, PendingFinish, SetChanges,
    SyncedPosition, TreesChanges, TreesState,
};
use koidrive_engine::tree::{FileIdentity, Kind, NodeId, Shape};
use koidrive_proto::v1::{Commit, CommitResult, Content, NodeKind};
use prost::{Message, Oneof};
use rusqlite::{CachedStatement, OptionalExtension, ToSql, params};

use crate::{Error, Result, Tx, decode, ino_columns, ino_from_columns, non_empty};

/// What the engine needs to continue where it stopped, besides the nodes of R, L and S, which the
/// store keeps per node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EngineState {
    pub trees: TreesState,
    pub commits: CommitsState,
    pub effects: EffectsState,
}

/// What changed in the engine state since it was last written, as [`Tx::write_engine_changes`] takes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EngineChanges {
    pub trees: TreesChanges,
    pub commits: CommitsChanges,
    pub effects: EffectsChanges,
}

impl EngineChanges {
    /// The changes that take `before` to `after`.
    pub fn between(before: &EngineState, after: &EngineState) -> Self {
        let EngineState {
            trees,
            commits,
            effects,
        } = after;
        Self {
            trees: TreesChanges::between(&before.trees, trees),
            commits: CommitsChanges::between(&before.commits, commits),
            effects: EffectsChanges::between(&before.effects, effects),
        }
    }
}

/// The node sets of the state, by the name `node_sets` stores them under.
fn node_sets(state: &mut EngineState) -> [(&'static str, &mut BTreeSet<NodeId>); 5] {
    [
        ("dirty", &mut state.trees.dirty),
        ("unsearched", &mut state.trees.unsearched),
        ("reverted", &mut state.trees.reverted),
        ("problems", &mut state.commits.problems),
        ("split", &mut state.commits.split),
    ]
}

/// The node maps of the state, by the name `node_maps` stores them under.
fn node_maps(state: &mut EngineState) -> [(&'static str, &mut BTreeMap<NodeId, NodeId>); 2] {
    [
        ("parked_as", &mut state.trees.parked_as),
        ("aliases", &mut state.commits.aliases),
    ]
}

impl Tx<'_> {
    /// The stored engine state, or the default state of a space that never stored one.
    pub fn engine_state(&self) -> Result<EngineState> {
        let mut state = EngineState::default();
        self.engine_state_unless(&mut state, &mut || false)?;
        Ok(state)
    }

    /// Reads the stored engine state into `state`, which starts as the default, and tells whether it read all of it.
    /// It stops once `abandoned`, which is asked once per row read, returns true.
    pub fn engine_state_unless(
        &self,
        state: &mut EngineState,
        abandoned: &mut dyn FnMut() -> bool,
    ) -> Result<bool> {
        let mut sets = node_sets(state);
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_set, node_id FROM node_sets")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let name: String = row.get(0)?;
            let (_, set) = sets
                .iter_mut()
                .find(|(set, _)| *set == name)
                .ok_or_else(|| invalid(format!("unknown node set {name:?}")))?;
            set.insert(NodeId::from(row.get::<_, String>(1)?));
        }
        let mut maps = node_maps(state);
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_map, node_id, target FROM node_maps")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let name: String = row.get(0)?;
            let (_, map) = maps
                .iter_mut()
                .find(|(map, _)| *map == name)
                .ok_or_else(|| invalid(format!("unknown node map {name:?}")))?;
            map.insert(
                NodeId::from(row.get::<_, String>(1)?),
                NodeId::from(row.get::<_, String>(2)?),
            );
        }

        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name, kind, content, slot, released FROM waiting_updates",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let shape = match row.get::<_, Option<String>>(2)? {
                None => None,
                Some(name) => Some(Shape {
                    parent: NodeId::from(row.get::<_, Option<String>>(1)?.unwrap_or_default()),
                    name,
                    kind: kind(row.get(3)?)?,
                    content: decode(row.get(4)?)?,
                }),
            };
            let origin = match (row.get::<_, Option<u64>>(5)?, row.get::<_, bool>(6)?) {
                (Some(slot), _) => Origin::Commit(slot),
                (None, true) => Origin::Released,
                (None, false) => Origin::Other,
            };
            state.trees.waiting.insert(
                NodeId::from(row.get::<_, String>(0)?),
                Waiting { shape, origin },
            );
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, changed FROM watched_contents")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .watched
                .insert(NodeId::from(row.get::<_, String>(0)?), row.get(1)?);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, rev, parent_id, name FROM watched_positions")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let parent = row.get::<_, Option<String>>(2)?.unwrap_or_default();
            let agreed = (row.get::<_, Option<u64>>(1)?)
                .zip(row.get::<_, Option<String>>(3)?)
                .map(|(rev, name)| Agreed {
                    rev,
                    parent: NodeId::from(parent),
                    name,
                });
            state
                .trees
                .watched_positions
                .insert(NodeId::from(row.get::<_, String>(0)?), agreed);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, seq FROM trashed_here")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .trashed_here
                .insert(NodeId::from(row.get::<_, String>(0)?), row.get(1)?);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, seq FROM removed_here")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .removed_here
                .insert(NodeId::from(row.get::<_, String>(0)?), row.get(1)?);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name, kind, content FROM discarded_nodes",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let shape = Shape {
                parent: NodeId::from(row.get::<_, Option<String>>(1)?.unwrap_or_default()),
                name: row.get(2)?,
                kind: kind(row.get(3)?)?,
                content: decode(row.get(4)?)?,
            };
            state
                .trees
                .discarded
                .insert(NodeId::from(row.get::<_, String>(0)?), shape);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, parent_id FROM removed_from")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state.trees.removed_from.insert(
                NodeId::from(row.get::<_, String>(0)?),
                NodeId::from(row.get::<_, String>(1)?),
            );
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name, kind, content FROM forgotten_shapes",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let shape = Shape {
                parent: NodeId::from(row.get::<_, Option<String>>(1)?.unwrap_or_default()),
                name: row.get(2)?,
                kind: kind(row.get(3)?)?,
                content: decode(row.get(4)?)?,
            };
            state
                .trees
                .forgotten_shapes
                .insert(NodeId::from(row.get::<_, String>(0)?), shape);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, parent_id, name, kind, content FROM lost_shapes")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let shape = Shape {
                parent: NodeId::from(row.get::<_, Option<String>>(1)?.unwrap_or_default()),
                name: row.get(2)?,
                kind: kind(row.get(3)?)?,
                content: decode(row.get(4)?)?,
            };
            state
                .trees
                .lost_shapes
                .insert(NodeId::from(row.get::<_, String>(0)?), shape);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id FROM published_nodes")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .commits
                .published
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = (self.0)
            .prepare_cached("SELECT node_id, content_rev, meta_rev, held_here FROM given_up")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let given = GivenUp {
                revisions: Revisions {
                    content_rev: row.get(1)?,
                    meta_rev: row.get(2)?,
                },
                held_here: row.get(3)?,
            };
            state
                .commits
                .given_up_nodes
                .insert(NodeId::from(row.get::<_, String>(0)?), given);
        }
        let mut stmt = (self.0).prepare_cached("SELECT node_id FROM answer_moves")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .commits
                .answer_moves
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, ino, ino_high, birth_ns, lost, handle FROM recorded_nodes",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let node: NodeId = NodeId::from(row.get::<_, String>(0)?);
            match row.get::<_, Option<i64>>(1)? {
                Some(ino) => {
                    let identity = FileIdentity {
                        ino: ino_from_columns(ino, row.get(2)?),
                        birth_ns: row.get(3)?,
                        handle: row.get::<_, Option<i64>>(5)?.map(|handle| handle as u64),
                    };
                    state.trees.displaced.insert(node, identity);
                }
                None => {
                    if row.get(4)? {
                        state.trees.lost.insert(node.clone());
                    }
                    state.trees.outside.insert(node);
                }
            }
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, seq FROM edited_here")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .edited_here
                .insert(NodeId::from(row.get::<_, String>(0)?), row.get(1)?);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, seq, upgraded FROM moved_here")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let moved = MovedHere {
                seq: row.get(1)?,
                upgraded: row.get(2)?,
            };
            state
                .trees
                .moved_here
                .insert(NodeId::from(row.get::<_, String>(0)?), moved);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, rev, parent_id, name FROM past_positions ORDER BY node_id, position",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let past = Agreed {
                rev: row.get(1)?,
                parent: NodeId::from(row.get::<_, Option<String>>(2)?.unwrap_or_default()),
                name: row.get(3)?,
            };
            let node: NodeId = NodeId::from(row.get::<_, String>(0)?);
            state
                .trees
                .past_positions
                .entry(node)
                .or_default()
                .push(past);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name FROM synced_positions ORDER BY node_id, position",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let position = SyncedPosition {
                parent: NodeId::from(row.get::<_, Option<String>>(1)?.unwrap_or_default()),
                name: row.get(2)?,
            };
            let node = NodeId::from(row.get::<_, String>(0)?);
            state
                .trees
                .synced_positions
                .entry(node)
                .or_default()
                .push(position);
        }
        let mut stmt = self.0.prepare_cached("SELECT node_id FROM unsent_edits")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .unsent_edits
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self.0.prepare_cached("SELECT node_id FROM dropped_edits")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .dropped_edits
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id FROM cycle_reverted")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .cycle_reverted
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self.0.prepare_cached("SELECT node_id FROM unobserved")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .unobserved
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self.0.prepare_cached("SELECT node_id FROM missing_nodes")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .missing
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self.0.prepare_cached("SELECT node_id FROM unsent_moves")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .unsent_moves
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self.0.prepare_cached("SELECT node_id FROM kept_folders")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .kept_folders
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id FROM unsent_creates")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .unsent_creates
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, entries FROM kept_entries")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let bytes: Vec<u8> = row.get(1)?;
            let kept = serde_json::from_slice(&bytes)
                .map_err(|error| invalid(format!("kept entries: {error}")))?;
            state
                .trees
                .kept_entries
                .insert(NodeId::from(row.get::<_, String>(0)?), kept);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, cause FROM kept_nodes")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let bytes: Option<Vec<u8>> = row.get(1)?;
            let cause = bytes.and_then(|bytes| serde_json::from_slice(&bytes).ok());
            state
                .trees
                .kept_nodes
                .insert(NodeId::from(row.get::<_, String>(0)?), cause);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id FROM unseen_changes")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .unseen_changes
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, version_id FROM replaced_versions ORDER BY node_id, position",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            (state
                .trees
                .replaced_versions
                .entry(NodeId::from(row.get::<_, String>(0)?))
                .or_default())
            .push(row.get(1)?);
        }
        let mut stmt = self.0.prepare_cached("SELECT node_id FROM resends")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .resends
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, provenance FROM content_provenance")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let bytes: Vec<u8> = row.get(1)?;
            let provenance: ContentProvenance = serde_json::from_slice(&bytes)
                .map_err(|error| invalid(format!("content provenance: {error}")))?;
            state
                .trees
                .content_provenance
                .insert(NodeId::from(row.get::<_, String>(0)?), provenance);
        }
        let mut stmt = self.0.prepare_cached("SELECT node_id FROM local_changes")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .local_changes
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, parent_id, name FROM lost_positions")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let at = LostPosition {
                parent: NodeId::from(row.get::<_, String>(1)?),
                name: row.get(2)?,
            };
            state
                .trees
                .lost_positions
                .insert(NodeId::from(row.get::<_, String>(0)?), at);
        }
        let mut stmt = self.0.prepare_cached("SELECT node_id FROM lost_creates")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .lost_creates
                .insert(NodeId::from(row.get::<_, String>(0)?));
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, kept FROM kept_originals")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state.trees.kept_originals.insert(
                NodeId::from(row.get::<_, String>(0)?),
                NodeId::from(row.get::<_, String>(1)?),
            );
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, trash_group FROM trash_groups")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .commits
                .trash_groups
                .insert(NodeId::from(row.get::<_, String>(0)?), row.get(1)?);
        }
        let mut stmt = (self.0).prepare_cached(
            "SELECT restore_id, trash_group, parent_id, name, folders, landed_parent_id, landed_name FROM split_restores",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let folders: String = row.get(4)?;
            let split = SplitRestore {
                trash_group: row.get(1)?,
                parent: NodeId::from(row.get::<_, String>(2)?),
                name: row.get(3)?,
                folders: serde_json::from_str(&folders)
                    .map_err(|error| invalid(format!("split restore folders: {error}")))?,
                landed: Option::zip(
                    row.get::<_, Option<String>>(5)?.map(NodeId::from),
                    row.get(6)?,
                ),
            };
            state.commits.split_restores.insert(row.get(0)?, split);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, pending_for FROM unsent_marks")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            state
                .trees
                .unsent_marks
                .insert(NodeId::from(row.get::<_, String>(0)?), row.get(1)?);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, pending_name, name, meta_rev, disk_first, base, sending FROM pending_finishes",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let finish = PendingFinish {
                parent: NodeId::from(row.get::<_, String>(1)?),
                pending: row.get(2)?,
                name: row.get(3)?,
                origin: match row.get(4)? {
                    Some(meta_rev) => FinishOrigin::Mark { meta_rev },
                    None => FinishOrigin::Legacy,
                },
                disk_first: row.get(5)?,
                base: row.get(6)?,
                sending: row.get(7)?,
            };
            state
                .trees
                .pending_finishes
                .insert(NodeId::from(row.get::<_, String>(0)?), finish);
        }

        if let Some((accepted, halt, cap)) = self
            .0
            .query_row(
                "SELECT accepted, halt, policy_code, recovery_trigger, restore_seq, restore_point_ms,
                        cap, recovery_policy_code
                 FROM commit_state WHERE id = 1",
                [],
                |row| {
                    let halt = HaltRow {
                        halt: row.get(1)?,
                        policy_code: row.get(2)?,
                        recovery_trigger: row.get(3)?,
                        restore_seq: row.get(4)?,
                        restore_point_ms: row.get(5)?,
                        recovery_policy_code: row.get(7)?,
                    };
                    Ok((row.get::<_, u64>(0)?, halt, row.get::<_, Option<usize>>(6)?))
                },
            )
            .optional()?
        {
            state.commits.accepted = accepted;
            state.commits.halted = halt.halt()?;
            state.commits.cap = cap;
        }
        if !self.in_flight(&mut state.commits.in_flight, abandoned)? {
            return Ok(false);
        }
        let (restores, resolved) = (&mut state.commits.restores, &mut state.commits.resolved);
        if !self.read_restores(restores, resolved, abandoned)? {
            return Ok(false);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT device_seq, seq, commit_id, committed_at_ms FROM commit_slots",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let made = Slot {
                seq: row.get(1)?,
                commit_id: row.get(2)?,
                committed_at_ms: row.get(3)?,
            };
            state.commits.slots.insert(row.get(0)?, made);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT device_seq, commit_ids FROM commit_slot_given_up")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let ids: String = row.get(1)?;
            let ids = ids.split_whitespace().map(str::to_owned).collect();
            state.commits.given_up.insert(row.get(0)?, ids);
        }
        if !self.open_questions(&mut state.commits.questions, abandoned)?
            || !self.guard(&mut state.commits.guard, abandoned)?
        {
            return Ok(false);
        }

        if let Some(cleared) = self
            .0
            .query_row("SELECT cleared FROM effect_state WHERE id = 1", [], |row| {
                row.get::<_, u64>(0)
            })
            .optional()?
        {
            state.effects.cleared = cleared;
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT record FROM effects_in_flight ORDER BY position")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let record = StoredRecord::decode(row.get::<_, Vec<u8>>(0)?.as_slice())?;
            state
                .effects
                .in_flight
                .push(EffectRecord::try_from(record)?);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, effect FROM failed_effects")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let effect = StoredEffect::decode(row.get::<_, Vec<u8>>(1)?.as_slice())?;
            state.effects.failed.insert(
                NodeId::from(row.get::<_, String>(0)?),
                Effect::try_from(effect)?,
            );
        }
        Ok(true)
    }

    /// Replaces the stored engine state with `state`.
    pub fn set_engine_state(&self, state: &EngineState) -> Result<()> {
        self.update_engine_state(&self.engine_state()?, state)
    }

    /// Changes the stored engine state from `before`, which must be the state the store holds, to
    /// `after`, writing what differs as [`Tx::write_engine_changes`] does.
    pub fn update_engine_state(&self, before: &EngineState, after: &EngineState) -> Result<()> {
        self.write_engine_changes(&EngineChanges::between(before, after))
    }

    /// Writes `changes` into the stored engine state. Only the rows of what changed are written: a
    /// member of a node set or a node map, a waiting update, a failed effect and each other entry by its key, removed
    /// when the state no longer holds it, and each of the commit counters, the commit in flight, the restores, the
    /// open questions, the effect counter and the effects in flight as a whole.
    pub fn write_engine_changes(&self, changes: &EngineChanges) -> Result<()> {
        let EngineChanges {
            trees,
            commits,
            effects,
        } = changes;
        let TreesChanges {
            dirty,
            unsearched,
            reverted,
            unobserved,
            parked_as,
            waiting,
            watched,
            watched_positions,
            trashed_here,
            removed_here,
            discarded,
            removed_from,
            forgotten_shapes,
            lost_shapes,
            outside,
            lost,
            displaced,
            missing,
            edited_here,
            moved_here,
            past_positions,
            synced_positions,
            unsent_edits,
            dropped_edits,
            cycle_reverted,
            unsent_moves,
            unsent_creates,
            kept_folders,
            kept_entries,
            kept_nodes,
            unseen_changes,
            replaced_versions,
            content_provenance,
            resends,
            local_changes,
            lost_positions,
            lost_creates,
            kept_originals,
            unsent_marks,
            pending_finishes,
        } = trees;
        let CommitsChanges {
            accepted,
            in_flight,
            problems,
            halted,
            cap,
            split,
            trash_groups,
            split_restores,
            aliases,
            restores,
            resolved,
            slots,
            given_up,
            questions,
            published,
            given_up_nodes,
            answer_moves,
            guard,
        } = commits;
        let EffectsChanges {
            in_flight: effect,
            cleared,
            failed,
        } = effects;

        let mut remove = self
            .0
            .prepare_cached("DELETE FROM node_sets WHERE node_set = ?1 AND node_id = ?2")?;
        let mut insert = self.0.prepare_cached(
            "INSERT OR IGNORE INTO node_sets (node_set, node_id) VALUES (?1, ?2)",
        )?;
        let sets = [
            ("dirty", dirty),
            ("unsearched", unsearched),
            ("reverted", reverted),
            ("problems", problems),
            ("split", split),
        ];
        for (name, changes) in sets {
            for (node, held) in changes {
                let node = node.to_string();
                match held {
                    true => insert.execute(params![name, node])?,
                    false => remove.execute(params![name, node])?,
                };
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM node_maps WHERE node_map = ?1 AND node_id = ?2")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO node_maps (node_map, node_id, target) VALUES (?1, ?2, ?3)",
        )?;
        for (name, changes) in [("parked_as", parked_as), ("aliases", aliases)] {
            for (node, target) in changes {
                let node = node.to_string();
                match target {
                    Some(target) => put.execute(params![name, node, target.to_string()])?,
                    None => remove.execute(params![name, node])?,
                };
            }
        }
        self.write_node_map(
            waiting,
            "DELETE FROM waiting_updates WHERE node_id = ?1",
            "INSERT OR REPLACE INTO waiting_updates (node_id, parent_id, name, kind, content, slot, released)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            |put, node, Waiting { shape, origin }| {
                let slot = match origin {
                    Origin::Commit(slot) => Some(*slot),
                    Origin::Other | Origin::Released => None,
                };
                put.execute(params![
                    node,
                    shape
                        .as_ref()
                        .map(|shape| shape.parent.to_string())
                        .filter(|parent| !parent.is_empty()),
                    shape.as_ref().map(|shape| &shape.name),
                    shape.as_ref().map(|shape| node_kind(shape.kind)),
                    shape
                        .as_ref()
                        .and_then(|shape| shape.content.as_ref())
                        .map(Content::encode_to_vec),
                    slot,
                    *origin == Origin::Released,
                ])
            },
        )?;
        self.write_node_map(
            watched,
            "DELETE FROM watched_contents WHERE node_id = ?1",
            "INSERT OR REPLACE INTO watched_contents (node_id, changed) VALUES (?1, ?2)",
            |put, node, took| put.execute(params![node, took]),
        )?;
        self.write_node_map(
            watched_positions,
            "DELETE FROM watched_positions WHERE node_id = ?1",
            "INSERT OR REPLACE INTO watched_positions (node_id, rev, parent_id, name)
             VALUES (?1, ?2, ?3, ?4)",
            |put, node, agreed| {
                put.execute(params![
                    node,
                    agreed.as_ref().map(|agreed| agreed.rev),
                    agreed
                        .as_ref()
                        .map(|agreed| agreed.parent.to_string())
                        .filter(|parent| !parent.is_empty()),
                    agreed.as_ref().map(|agreed| &agreed.name),
                ])
            },
        )?;
        self.write_node_map(
            trashed_here,
            "DELETE FROM trashed_here WHERE node_id = ?1",
            "INSERT OR REPLACE INTO trashed_here (node_id, seq) VALUES (?1, ?2)",
            |put, node, seq| put.execute(params![node, seq]),
        )?;
        self.write_node_map(
            removed_here,
            "DELETE FROM removed_here WHERE node_id = ?1",
            "INSERT OR REPLACE INTO removed_here (node_id, seq) VALUES (?1, ?2)",
            |put, node, seq| put.execute(params![node, seq]),
        )?;
        self.write_node_map(
            discarded,
            "DELETE FROM discarded_nodes WHERE node_id = ?1",
            "INSERT OR REPLACE INTO discarded_nodes (node_id, parent_id, name, kind, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            |put, node, shape| {
                put.execute(params![
                    node,
                    non_empty(&shape.parent.to_string()),
                    shape.name,
                    node_kind(shape.kind),
                    shape.content.as_ref().map(Content::encode_to_vec),
                ])
            },
        )?;
        self.write_node_map(
            removed_from,
            "DELETE FROM removed_from WHERE node_id = ?1",
            "INSERT OR REPLACE INTO removed_from (node_id, parent_id) VALUES (?1, ?2)",
            |put, node, folder| put.execute(params![node, folder.to_string()]),
        )?;
        self.write_node_map(
            forgotten_shapes,
            "DELETE FROM forgotten_shapes WHERE node_id = ?1",
            "INSERT OR REPLACE INTO forgotten_shapes (node_id, parent_id, name, kind, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            |put, node, shape| {
                put.execute(params![
                    node,
                    non_empty(&shape.parent.to_string()),
                    shape.name,
                    node_kind(shape.kind),
                    shape.content.as_ref().map(Content::encode_to_vec),
                ])
            },
        )?;
        self.write_node_map(
            lost_shapes,
            "DELETE FROM lost_shapes WHERE node_id = ?1",
            "INSERT OR REPLACE INTO lost_shapes (node_id, parent_id, name, kind, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            |put, node, shape| {
                put.execute(params![
                    node,
                    non_empty(&shape.parent.to_string()),
                    shape.name,
                    node_kind(shape.kind),
                    shape.content.as_ref().map(Content::encode_to_vec),
                ])
            },
        )?;
        self.write_recorded(outside, lost, displaced)?;
        self.write_set(
            missing,
            "DELETE FROM missing_nodes WHERE node_id = ?1",
            "INSERT OR IGNORE INTO missing_nodes (node_id) VALUES (?1)",
        )?;
        self.write_node_map(
            edited_here,
            "DELETE FROM edited_here WHERE node_id = ?1",
            "INSERT OR REPLACE INTO edited_here (node_id, seq) VALUES (?1, ?2)",
            |put, node, seq| put.execute(params![node, seq]),
        )?;
        self.write_set(
            unsent_edits,
            "DELETE FROM unsent_edits WHERE node_id = ?1",
            "INSERT OR IGNORE INTO unsent_edits (node_id) VALUES (?1)",
        )?;
        self.write_set(
            dropped_edits,
            "DELETE FROM dropped_edits WHERE node_id = ?1",
            "INSERT OR IGNORE INTO dropped_edits (node_id) VALUES (?1)",
        )?;
        self.write_set(
            cycle_reverted,
            "DELETE FROM cycle_reverted WHERE node_id = ?1",
            "INSERT OR IGNORE INTO cycle_reverted (node_id) VALUES (?1)",
        )?;
        self.write_set(
            unobserved,
            "DELETE FROM unobserved WHERE node_id = ?1",
            "INSERT OR IGNORE INTO unobserved (node_id) VALUES (?1)",
        )?;
        self.write_set(
            unsent_moves,
            "DELETE FROM unsent_moves WHERE node_id = ?1",
            "INSERT OR IGNORE INTO unsent_moves (node_id) VALUES (?1)",
        )?;
        self.write_set(
            unsent_creates,
            "DELETE FROM unsent_creates WHERE node_id = ?1",
            "INSERT OR IGNORE INTO unsent_creates (node_id) VALUES (?1)",
        )?;
        self.write_set(
            kept_folders,
            "DELETE FROM kept_folders WHERE node_id = ?1",
            "INSERT OR IGNORE INTO kept_folders (node_id) VALUES (?1)",
        )?;
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM kept_entries WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO kept_entries (node_id, entries) VALUES (?1, ?2)",
        )?;
        for (node, kept) in kept_entries {
            let node = node.to_string();
            match kept {
                Some(kept) => {
                    let bytes = serde_json::to_vec(kept)
                        .map_err(|error| invalid(format!("kept entries: {error}")))?;
                    put.execute(params![node, bytes])?;
                }
                None => {
                    remove.execute([node])?;
                }
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM kept_nodes WHERE node_id = ?1")?;
        let mut put = self
            .0
            .prepare_cached("INSERT OR REPLACE INTO kept_nodes (node_id, cause) VALUES (?1, ?2)")?;
        for (node, kept) in kept_nodes {
            let node = node.to_string();
            match kept {
                Some(cause) => {
                    let bytes = (cause.as_ref().map(serde_json::to_vec).transpose())
                        .map_err(|error| invalid(format!("kept cause: {error}")))?;
                    put.execute(params![node, bytes])?;
                }
                None => {
                    remove.execute([node])?;
                }
            }
        }
        self.write_set(
            unseen_changes,
            "DELETE FROM unseen_changes WHERE node_id = ?1",
            "INSERT OR IGNORE INTO unseen_changes (node_id) VALUES (?1)",
        )?;
        self.write_node_map(
            moved_here,
            "DELETE FROM moved_here WHERE node_id = ?1",
            "INSERT OR REPLACE INTO moved_here (node_id, seq, upgraded) VALUES (?1, ?2, ?3)",
            |put, node, moved| put.execute(params![node, moved.seq, moved.upgraded]),
        )?;
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM past_positions WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT INTO past_positions (node_id, position, rev, parent_id, name)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (node, past) in past_positions {
            let node = node.to_string();
            remove.execute([&node])?;
            for (index, position) in past.iter().flatten().enumerate() {
                put.execute(params![
                    node,
                    index,
                    position.rev,
                    non_empty(&position.parent.to_string()),
                    &position.name
                ])?;
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM synced_positions WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT INTO synced_positions (node_id, position, parent_id, name) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (node, positions) in synced_positions {
            let node = node.to_string();
            remove.execute([&node])?;
            for (index, position) in positions.iter().flatten().enumerate() {
                put.execute(params![
                    node,
                    index,
                    non_empty(&position.parent.to_string()),
                    &position.name
                ])?;
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM replaced_versions WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT INTO replaced_versions (node_id, position, version_id) VALUES (?1, ?2, ?3)",
        )?;
        for (node, versions) in replaced_versions {
            let node = node.to_string();
            remove.execute([&node])?;
            for (position, version) in versions.iter().flatten().enumerate() {
                put.execute(params![node, position, version])?;
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM content_provenance WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO content_provenance (node_id, provenance) VALUES (?1, ?2)",
        )?;
        for (node, provenance) in content_provenance {
            let node = node.to_string();
            match provenance {
                Some(provenance) => {
                    let bytes = serde_json::to_vec(provenance)
                        .map_err(|error| invalid(format!("content provenance: {error}")))?;
                    put.execute(params![node, bytes])?;
                }
                None => {
                    remove.execute([node])?;
                }
            }
        }
        self.write_set(
            resends,
            "DELETE FROM resends WHERE node_id = ?1",
            "INSERT OR IGNORE INTO resends (node_id) VALUES (?1)",
        )?;
        self.write_set(
            local_changes,
            "DELETE FROM local_changes WHERE node_id = ?1",
            "INSERT OR IGNORE INTO local_changes (node_id) VALUES (?1)",
        )?;
        self.write_set(
            lost_creates,
            "DELETE FROM lost_creates WHERE node_id = ?1",
            "INSERT OR IGNORE INTO lost_creates (node_id) VALUES (?1)",
        )?;
        self.write_node_map(
            lost_positions,
            "DELETE FROM lost_positions WHERE node_id = ?1",
            "INSERT OR REPLACE INTO lost_positions (node_id, parent_id, name) VALUES (?1, ?2, ?3)",
            |put, node, at: &LostPosition| {
                put.execute(params![node, at.parent.to_string(), at.name])
            },
        )?;
        self.write_node_map(
            kept_originals,
            "DELETE FROM kept_originals WHERE node_id = ?1",
            "INSERT OR REPLACE INTO kept_originals (node_id, kept) VALUES (?1, ?2)",
            |put, node, kept| put.execute(params![node, kept.to_string()]),
        )?;
        self.write_node_map(
            unsent_marks,
            "DELETE FROM unsent_marks WHERE node_id = ?1",
            "INSERT OR REPLACE INTO unsent_marks (node_id, pending_for) VALUES (?1, ?2)",
            |put, node, name| put.execute(params![node, name]),
        )?;
        self.write_node_map(
            pending_finishes,
            "DELETE FROM pending_finishes WHERE node_id = ?1",
            "INSERT OR REPLACE INTO pending_finishes
             (node_id, parent_id, pending_name, name, meta_rev, disk_first, base, sending)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            |put, node, finish| {
                put.execute(params![
                    node,
                    finish.parent.to_string(),
                    finish.pending,
                    finish.name,
                    match finish.origin {
                        FinishOrigin::Mark { meta_rev } => Some(meta_rev),
                        FinishOrigin::Legacy => None,
                    },
                    finish.disk_first,
                    finish.base,
                    finish.sending,
                ])
            },
        )?;

        if accepted.is_some() || halted.is_some() || cap.is_some() {
            self.0.execute(
                "INSERT OR IGNORE INTO commit_state (id, accepted) VALUES (1, 0)",
                [],
            )?;
        }
        if let Some(accepted) = accepted {
            self.0.execute(
                "UPDATE commit_state SET accepted = ?1 WHERE id = 1",
                [accepted],
            )?;
        }
        if let Some(halted) = halted {
            let halt = HaltRow::of(*halted);
            self.0.execute(
                "UPDATE commit_state
                 SET halt = ?1, policy_code = ?2, recovery_trigger = ?3, restore_seq = ?4,
                     restore_point_ms = ?5, recovery_policy_code = ?6
                 WHERE id = 1",
                params![
                    halt.halt,
                    halt.policy_code,
                    halt.recovery_trigger,
                    halt.restore_seq,
                    halt.restore_point_ms,
                    halt.recovery_policy_code
                ],
            )?;
        }
        if let Some(cap) = cap {
            self.0
                .execute("UPDATE commit_state SET cap = ?1 WHERE id = 1", [cap])?;
        }
        if let Some(in_flight) = in_flight {
            for table in [
                "commit_in_flight",
                "commit_in_flight_deletes",
                "commit_in_flight_delete_members",
                "commit_in_flight_restore_members",
                "commit_in_flight_restore_owners",
            ] {
                self.0.execute(&format!("DELETE FROM {table}"), [])?;
            }
            if let Some(flight) = in_flight {
                self.0.execute(
                    "INSERT INTO commit_in_flight (id, commit_message, result) VALUES (1, ?1, ?2)",
                    params![
                        flight.commit.encode_to_vec(),
                        flight.result.as_ref().map(CommitResult::encode_to_vec),
                    ],
                )?;
                let mut targets = self
                    .0
                    .prepare_cached("INSERT INTO commit_in_flight_deletes (target) VALUES (?1)")?;
                let mut members = self.0.prepare_cached(
                    "INSERT INTO commit_in_flight_delete_members (target, position, node_id) VALUES (?1, ?2, ?3)",
                )?;
                for (target, nodes) in &flight.deletes {
                    let target = target.to_string();
                    targets.execute([&target])?;
                    for (position, node) in nodes.iter().enumerate() {
                        members.execute(params![target, position, node.to_string()])?;
                    }
                }
                let mut restored = self.0.prepare_cached(
                    "INSERT INTO commit_in_flight_restore_members (op, position, node_id) VALUES (?1, ?2, ?3)",
                )?;
                for (index, nodes) in &flight.restores {
                    for (position, node) in nodes.iter().enumerate() {
                        restored.execute(params![index, position, node.to_string()])?;
                    }
                }
                let mut owners = self.0.prepare_cached(
                    "INSERT INTO commit_in_flight_restore_owners (op, restore_id) VALUES (?1, ?2)",
                )?;
                for (index, id) in &flight.owners {
                    owners.execute(params![index, id])?;
                }
            }
        }
        self.write_map(
            slots,
            "DELETE FROM commit_slots WHERE device_seq = ?1",
            "INSERT OR REPLACE INTO commit_slots (device_seq, seq, commit_id, committed_at_ms) VALUES (?1, ?2, ?3, ?4)",
            |put, slot, made| {
                put.execute(params![slot, made.seq, made.commit_id, made.committed_at_ms])
            },
        )?;
        self.write_map(
            given_up,
            "DELETE FROM commit_slot_given_up WHERE device_seq = ?1",
            "INSERT OR REPLACE INTO commit_slot_given_up (device_seq, commit_ids) VALUES (?1, ?2)",
            |put, slot, ids| put.execute(params![slot, ids.join(" ")]),
        )?;
        if restores.is_some() {
            self.0
                .execute("DELETE FROM restore_intents WHERE outcome IS NULL", [])?;
        }
        if resolved.is_some() {
            self.0
                .execute("DELETE FROM restore_intents WHERE outcome IS NOT NULL", [])?;
        }
        if restores.is_some() || resolved.is_some() {
            let queued = restores.clone().unwrap_or_default();
            self.set_restores(&queued, resolved.as_deref().unwrap_or_default())?;
        }
        if let Some(questions) = questions {
            self.set_open_questions(questions)?;
        }
        self.write_node_map(
            trash_groups,
            "DELETE FROM trash_groups WHERE node_id = ?1",
            "INSERT OR REPLACE INTO trash_groups (node_id, trash_group) VALUES (?1, ?2)",
            |put, node, group| put.execute(params![node, group]),
        )?;
        self.write_map(
            split_restores,
            "DELETE FROM split_restores WHERE restore_id = ?1",
            "INSERT OR REPLACE INTO split_restores (restore_id, trash_group, parent_id, name, folders, landed_parent_id, landed_name) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            |put, id, split| {
                let folders = serde_json::to_string(&split.folders).expect("node ids encode");
                let (landed_parent, landed_name) = split.landed.clone().unzip();
                put.execute(params![id, split.trash_group, split.parent.to_string(), split.name, folders, landed_parent.map(String::from), landed_name])
            },
        )?;
        self.write_set(
            published,
            "DELETE FROM published_nodes WHERE node_id = ?1",
            "INSERT OR IGNORE INTO published_nodes (node_id) VALUES (?1)",
        )?;
        self.write_node_map(
            given_up_nodes,
            "DELETE FROM given_up WHERE node_id = ?1",
            "INSERT OR REPLACE INTO given_up (node_id, content_rev, meta_rev, held_here) VALUES (?1, ?2, ?3, ?4)",
            |insert, node, given| {
                let revisions = given.revisions;
                insert.execute(params![node, revisions.content_rev, revisions.meta_rev, given.held_here])
            },
        )?;
        self.write_set(
            answer_moves,
            "DELETE FROM answer_moves WHERE node_id = ?1",
            "INSERT OR IGNORE INTO answer_moves (node_id) VALUES (?1)",
        )?;
        self.write_guard(guard)?;

        if effect.is_some() || cleared.is_some() {
            self.0.execute(
                "INSERT OR IGNORE INTO effect_state (id, cleared) VALUES (1, 0)",
                [],
            )?;
        }
        if let Some(cleared) = cleared {
            self.0.execute(
                "UPDATE effect_state SET cleared = ?1 WHERE id = 1",
                [cleared],
            )?;
        }
        if let Some(effects) = effect {
            self.0.execute("DELETE FROM effects_in_flight", [])?;
            let mut put = self.0.prepare_cached(
                "INSERT INTO effects_in_flight (position, record) VALUES (?1, ?2)",
            )?;
            for (position, record) in effects.iter().enumerate() {
                put.execute(params![
                    position as u64,
                    StoredRecord::from(record).encode_to_vec()
                ])?;
            }
        }
        self.write_node_map(
            failed,
            "DELETE FROM failed_effects WHERE node_id = ?1",
            "INSERT OR REPLACE INTO failed_effects (node_id, effect) VALUES (?1, ?2)",
            |put, node, effect| {
                put.execute(params![node, StoredEffect::from(effect).encode_to_vec()])
            },
        )?;
        Ok(())
    }

    /// The rows of the nodes kept outside the synced folders and of the displaced ones, which share a table: the
    /// rows that leave go first, so that a node that turns from one into the other takes its new row.
    fn write_recorded(
        &self,
        outside: &SetChanges<NodeId>,
        lost: &SetChanges<NodeId>,
        displaced: &MapChanges<NodeId, FileIdentity>,
    ) -> Result<()> {
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM recorded_nodes WHERE node_id = ?1 AND displaced = ?2")?;
        let left = (outside.iter().filter(|(_, held)| !held)).map(|(node, _)| (node, false));
        let gone =
            (displaced.iter().filter(|(_, held)| held.is_none())).map(|(node, _)| (node, true));
        for (node, displaced) in left.chain(gone) {
            remove.execute(params![node.to_string(), displaced])?;
        }
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO recorded_nodes (node_id, displaced, ino, ino_high, birth_ns, lost, handle)
             VALUES (?1, 1, ?2, ?3, ?4, 0, ?5)",
        )?;
        for (node, held) in displaced {
            if let Some(held) = held {
                let (ino, ino_high) = ino_columns(held.ino);
                put.execute(params![
                    node.to_string(),
                    ino,
                    ino_high,
                    held.birth_ns,
                    held.handle.map(|handle| handle as i64)
                ])?;
            }
        }
        let mut put = self.0.prepare_cached(
            "INSERT OR IGNORE INTO recorded_nodes (node_id, displaced, ino_high, lost) VALUES (?1, 0, 0, 0)",
        )?;
        for (node, _) in outside.iter().filter(|(_, held)| *held) {
            let node = node.to_string();
            put.execute([node])?;
        }
        let mut mark = self.0.prepare_cached(
            "UPDATE recorded_nodes SET lost = ?2 WHERE node_id = ?1 AND displaced = 0",
        )?;
        for (node, held) in lost {
            let node = node.to_string();
            mark.execute(params![node, held])?;
        }
        Ok(())
    }

    fn write_set(&self, changes: &SetChanges<NodeId>, remove: &str, insert: &str) -> Result<()> {
        let mut remove = self.0.prepare_cached(remove)?;
        let mut insert = self.0.prepare_cached(insert)?;
        for (node, held) in changes {
            match held {
                true => insert.execute([node.to_string()])?,
                false => remove.execute([node.to_string()])?,
            };
        }
        Ok(())
    }

    /// Writes the entries of `changes`: `put` writes each one the state holds with the statement `insert`, and
    /// `remove` removes the row of each key it no longer holds.
    pub(crate) fn write_map<K: ToSql, V>(
        &self,
        changes: &MapChanges<K, V>,
        remove: &str,
        insert: &str,
        mut put: impl FnMut(&mut CachedStatement<'_>, &K, &V) -> rusqlite::Result<usize>,
    ) -> Result<()> {
        let mut remove = self.0.prepare_cached(remove)?;
        let mut insert = self.0.prepare_cached(insert)?;
        for (key, value) in changes {
            match value {
                Some(value) => put(&mut insert, key, value)?,
                None => remove.execute([key])?,
            };
        }
        Ok(())
    }

    pub(crate) fn write_node_map<V>(
        &self,
        changes: &MapChanges<NodeId, V>,
        remove: &str,
        insert: &str,
        mut put: impl FnMut(&mut CachedStatement<'_>, &str, &V) -> rusqlite::Result<usize>,
    ) -> Result<()> {
        let mut remove = self.0.prepare_cached(remove)?;
        let mut insert = self.0.prepare_cached(insert)?;
        for (key, value) in changes {
            let key = key.to_string();
            match value {
                Some(value) => put(&mut insert, &key, value)?,
                None => remove.execute([&key])?,
            };
        }
        Ok(())
    }

    fn in_flight(
        &self,
        into: &mut Option<InFlight>,
        abandoned: &mut dyn FnMut() -> bool,
    ) -> Result<bool> {
        let Some((commit, result)) = self
            .0
            .query_row(
                "SELECT commit_message, result FROM commit_in_flight WHERE id = 1",
                [],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
            )
            .optional()?
        else {
            return Ok(true);
        };
        let mut deletes: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        let mut stmt = self
            .0
            .prepare_cached("SELECT target FROM commit_in_flight_deletes")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            deletes.insert(NodeId::from(row.get::<_, String>(0)?), Vec::new());
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT target, node_id FROM commit_in_flight_delete_members ORDER BY target, position",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let target: NodeId = NodeId::from(row.get::<_, String>(0)?);
            let Some(nodes) = deletes.get_mut(&target) else {
                return Err(invalid(format!(
                    "delete members for {target}, which is not a delete target"
                )));
            };
            nodes.push(NodeId::from(row.get::<_, String>(1)?));
        }
        let mut restores: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
        let mut stmt = self.0.prepare_cached(
            "SELECT op, node_id FROM commit_in_flight_restore_members ORDER BY op, position",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            restores
                .entry(row.get(0)?)
                .or_default()
                .push(NodeId::from(row.get::<_, String>(1)?));
        }
        let mut owners = BTreeMap::new();
        let mut stmt = self
            .0
            .prepare_cached("SELECT op, restore_id FROM commit_in_flight_restore_owners")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            owners.insert(row.get(0)?, row.get(1)?);
        }
        *into = Some(InFlight {
            commit: Commit::decode(commit.as_slice())?,
            deletes,
            restores,
            owners,
            result: decode(result)?,
        });
        Ok(true)
    }
}

fn invalid(what: String) -> Error {
    Error::InvalidEngineState(what)
}

#[derive(Default)]
struct HaltRow {
    halt: Option<String>,
    policy_code: Option<i32>,
    recovery_trigger: Option<String>,
    restore_seq: Option<u64>,
    restore_point_ms: Option<i64>,
    recovery_policy_code: Option<i32>,
}

impl HaltRow {
    fn of(halt: Option<Halt>) -> HaltRow {
        let named = |name: &str| Some(name.to_owned());
        match halt {
            None => HaltRow::default(),
            Some(Halt::ReadOnly) => HaltRow {
                halt: named("read_only"),
                ..HaltRow::default()
            },
            Some(Halt::Policy { code }) => HaltRow {
                halt: named("policy"),
                policy_code: Some(code),
                ..HaltRow::default()
            },
            Some(Halt::Recovery { cause, policy }) => HaltRow {
                halt: named("recovery"),
                recovery_trigger: named(match cause.trigger {
                    RecoveryTrigger::SpaceRewound => "space_rewound",
                    RecoveryTrigger::StoreRolledBack => "store_rolled_back",
                    RecoveryTrigger::StoreOlderThanDisk => "store_older_than_disk",
                }),
                restore_seq: cause.restore_seq,
                restore_point_ms: cause.restore_point_ms,
                recovery_policy_code: policy,
                ..HaltRow::default()
            },
        }
    }

    fn halt(self) -> Result<Option<Halt>> {
        let HaltRow {
            halt,
            policy_code,
            recovery_trigger,
            restore_seq,
            restore_point_ms,
            recovery_policy_code,
        } = self;
        let trigger = match recovery_trigger.as_deref() {
            None => None,
            Some("space_rewound") => Some(RecoveryTrigger::SpaceRewound),
            Some("store_rolled_back") => Some(RecoveryTrigger::StoreRolledBack),
            Some("store_older_than_disk") => Some(RecoveryTrigger::StoreOlderThanDisk),
            Some(other) => return Err(invalid(format!("recovery trigger {other:?}"))),
        };
        Ok(match (halt.as_deref(), policy_code, trigger) {
            (None, None, None) => None,
            (Some("read_only"), None, None) => Some(Halt::ReadOnly),
            (Some("recovery"), None, Some(trigger)) => Some(Halt::Recovery {
                cause: RecoveryCause {
                    trigger,
                    restore_seq,
                    restore_point_ms,
                },
                policy: recovery_policy_code,
            }),
            (Some("policy"), Some(code), None) => Some(Halt::Policy { code }),
            _ => {
                return Err(invalid(format!(
                    "halt {halt:?} with code {policy_code:?} and trigger {trigger:?}"
                )));
            }
        })
    }
}

pub(crate) fn node_kind(kind: Kind) -> i32 {
    match kind {
        Kind::Dir => NodeKind::Dir as i32,
        Kind::File => NodeKind::File as i32,
    }
}

pub(crate) fn kind(value: i32) -> Result<Kind> {
    match NodeKind::try_from(value) {
        Ok(NodeKind::Dir) => Ok(Kind::Dir),
        Ok(NodeKind::File) => Ok(Kind::File),
        _ => Err(invalid(format!("node kind {value}"))),
    }
}

fn required<T>(value: Option<T>, what: &str) -> Result<T> {
    value.ok_or_else(|| invalid(format!("{what} is missing")))
}

/// An [`EffectRecord`] as `effects_in_flight.record` stores it. The effect types are the engine's
/// own and have no wire format, so these messages give them one here. Their tags never change
/// meaning once shipped.
#[derive(Clone, PartialEq, Message)]
struct StoredRecord {
    #[prost(string, tag = "1")]
    effect_id: String,
    #[prost(string, tag = "2")]
    node: String,
    #[prost(uint64, tag = "3")]
    counter: u64,
    #[prost(message, optional, tag = "4")]
    effect: Option<StoredEffect>,
    #[prost(message, optional, tag = "5")]
    after: Option<StoredShape>,
    #[prost(message, optional, tag = "6")]
    staged: Option<StoredIdentity>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredEffect {
    #[prost(oneof = "Action", tags = "1, 2, 3, 4, 5")]
    action: Option<Action>,
}

#[derive(Clone, PartialEq, Oneof)]
enum Action {
    #[prost(message, tag = "1")]
    CreateDir(StoredLocation),
    #[prost(message, tag = "2")]
    WriteFile(StoredWrite),
    #[prost(message, tag = "3")]
    Move(StoredMove),
    #[prost(message, tag = "4")]
    RemoveFile(StoredDisplaced),
    #[prost(message, tag = "5")]
    RemoveDir(StoredRemoveDir),
}

#[derive(Clone, PartialEq, Message)]
struct StoredWrite {
    #[prost(message, optional, tag = "1")]
    at: Option<StoredLocation>,
    #[prost(message, optional, tag = "2")]
    content: Option<Content>,
    #[prost(message, optional, tag = "3")]
    replaces: Option<StoredDisplaced>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredMove {
    #[prost(message, optional, tag = "1")]
    from: Option<StoredLocation>,
    #[prost(message, optional, tag = "2")]
    identity: Option<StoredIdentity>,
    #[prost(message, optional, tag = "3")]
    to: Option<StoredLocation>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredRemoveDir {
    #[prost(message, optional, tag = "1")]
    at: Option<StoredLocation>,
    #[prost(message, optional, tag = "2")]
    identity: Option<StoredIdentity>,
    #[prost(message, repeated, tag = "3")]
    along: Vec<StoredDisplaced>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredDisplaced {
    #[prost(message, optional, tag = "1")]
    at: Option<StoredLocation>,
    #[prost(message, optional, tag = "2")]
    identity: Option<StoredIdentity>,
    #[prost(message, optional, tag = "3")]
    content: Option<Content>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredLocation {
    #[prost(oneof = "Place", tags = "1, 2")]
    place: Option<Place>,
}

#[derive(Clone, PartialEq, Oneof)]
enum Place {
    /// `.koidrive/parked/<node>` itself.
    #[prost(string, tag = "1")]
    Parked(String),
    #[prost(message, tag = "2")]
    At(StoredAt),
}

#[derive(Clone, PartialEq, Message)]
struct StoredAt {
    /// The parked directory the folders start at, by its node; the root when absent.
    #[prost(message, optional, tag = "1")]
    parked: Option<StoredDir>,
    #[prost(message, repeated, tag = "2")]
    dirs: Vec<StoredDir>,
    #[prost(string, tag = "3")]
    name: String,
}

#[derive(Clone, PartialEq, Message)]
struct StoredDir {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(message, optional, tag = "2")]
    identity: Option<StoredIdentity>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredIdentity {
    #[prost(uint64, tag = "2")]
    ino: u64,
    #[prost(int64, optional, tag = "3")]
    birth_ns: Option<i64>,
    #[prost(uint64, tag = "4")]
    ino_high: u64,
    #[prost(uint64, optional, tag = "5")]
    handle: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredShape {
    #[prost(string, tag = "1")]
    parent: String,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(int32, tag = "3")]
    kind: i32,
    #[prost(message, optional, tag = "4")]
    content: Option<Content>,
}

impl From<&EffectRecord> for StoredRecord {
    fn from(record: &EffectRecord) -> Self {
        Self {
            effect_id: record.effect_id.clone(),
            node: record.node.to_string(),
            counter: record.counter,
            effect: Some((&record.effect).into()),
            after: record.after.as_ref().map(|shape| StoredShape {
                parent: shape.parent.to_string(),
                name: shape.name.clone(),
                kind: node_kind(shape.kind),
                content: shape.content.clone(),
            }),
            staged: record.staged.as_ref().map(StoredIdentity::from),
        }
    }
}

impl TryFrom<StoredRecord> for EffectRecord {
    type Error = Error;

    fn try_from(record: StoredRecord) -> Result<Self> {
        let after = match record.after {
            None => None,
            Some(shape) => Some(Shape {
                parent: NodeId::from(shape.parent),
                name: shape.name,
                kind: kind(shape.kind)?,
                content: shape.content,
            }),
        };
        Ok(Self {
            effect_id: record.effect_id,
            node: NodeId::from(record.node),
            counter: record.counter,
            effect: required(record.effect, "the effect")?.try_into()?,
            after,
            recoveries: 0,
            staged: record.staged.map(FileIdentity::from),
        })
    }
}

impl From<&Effect> for StoredEffect {
    fn from(effect: &Effect) -> Self {
        let action = match effect {
            Effect::CreateDir { at } => Action::CreateDir(at.into()),
            Effect::WriteFile {
                at,
                content,
                replaces,
            } => Action::WriteFile(StoredWrite {
                at: Some(at.into()),
                content: Some(content.clone()),
                replaces: replaces.as_ref().map(Into::into),
            }),
            Effect::Move { from, identity, to } => Action::Move(StoredMove {
                from: Some(from.into()),
                identity: Some(identity.into()),
                to: Some(to.into()),
            }),
            Effect::RemoveFile { displaced } => Action::RemoveFile(displaced.into()),
            Effect::RemoveDir {
                at,
                identity,
                along,
            } => Action::RemoveDir(StoredRemoveDir {
                at: Some(at.into()),
                identity: Some(identity.into()),
                along: along.iter().map(Into::into).collect(),
            }),
        };
        Self {
            action: Some(action),
        }
    }
}

impl TryFrom<StoredEffect> for Effect {
    type Error = Error;

    fn try_from(effect: StoredEffect) -> Result<Self> {
        Ok(match required(effect.action, "the effect's action")? {
            Action::CreateDir(at) => Effect::CreateDir { at: at.try_into()? },
            Action::WriteFile(write) => Effect::WriteFile {
                at: required(write.at, "the written location")?.try_into()?,
                content: required(write.content, "the written content")?,
                replaces: write.replaces.map(TryInto::try_into).transpose()?,
            },
            Action::Move(moved) => Effect::Move {
                from: required(moved.from, "the move's source")?.try_into()?,
                identity: required(moved.identity, "the moved identity")?.into(),
                to: required(moved.to, "the move's target")?.try_into()?,
            },
            Action::RemoveFile(displaced) => Effect::RemoveFile {
                displaced: displaced.try_into()?,
            },
            Action::RemoveDir(removed) => Effect::RemoveDir {
                at: required(removed.at, "the removed location")?.try_into()?,
                identity: required(removed.identity, "the removed identity")?.into(),
                along: (removed.along.into_iter())
                    .map(TryInto::try_into)
                    .collect::<Result<_>>()?,
            },
        })
    }
}

impl From<&Displaced> for StoredDisplaced {
    fn from(displaced: &Displaced) -> Self {
        Self {
            at: Some((&displaced.at).into()),
            identity: Some((&displaced.identity).into()),
            content: Some(displaced.content.clone()),
        }
    }
}

impl TryFrom<StoredDisplaced> for Displaced {
    type Error = Error;

    fn try_from(displaced: StoredDisplaced) -> Result<Self> {
        Ok(Self {
            at: required(displaced.at, "the displaced location")?.try_into()?,
            identity: required(displaced.identity, "the displaced identity")?.into(),
            content: required(displaced.content, "the displaced content")?,
        })
    }
}

impl From<&Location> for StoredLocation {
    fn from(location: &Location) -> Self {
        let place = match location {
            Location::Parked(node) => Place::Parked(node.to_string()),
            Location::At { base, dirs, name } => Place::At(StoredAt {
                parked: match base {
                    Base::Root => None,
                    Base::Parked { node, identity } => Some(StoredDir {
                        name: node.to_string(),
                        identity: Some(identity.into()),
                    }),
                },
                dirs: dirs
                    .iter()
                    .map(|dir| StoredDir {
                        name: dir.name.clone(),
                        identity: Some((&dir.identity).into()),
                    })
                    .collect(),
                name: name.clone(),
            }),
        };
        Self { place: Some(place) }
    }
}

impl TryFrom<StoredLocation> for Location {
    type Error = Error;

    fn try_from(location: StoredLocation) -> Result<Self> {
        Ok(match required(location.place, "the location")? {
            Place::Parked(node) => Location::Parked(NodeId::from(node)),
            Place::At(at) => Location::At {
                base: match at.parked {
                    None => Base::Root,
                    Some(dir) => Base::Parked {
                        node: NodeId::from(dir.name),
                        identity: required(dir.identity, "the parked directory's identity")?.into(),
                    },
                },
                dirs: at
                    .dirs
                    .into_iter()
                    .map(|dir| {
                        Ok(Dir {
                            name: dir.name,
                            identity: required(dir.identity, "a directory's identity")?.into(),
                        })
                    })
                    .collect::<Result<_>>()?,
                name: at.name,
            },
        })
    }
}

impl From<&FileIdentity> for StoredIdentity {
    fn from(identity: &FileIdentity) -> Self {
        Self {
            ino: identity.ino as u64,
            birth_ns: identity.birth_ns,
            ino_high: (identity.ino >> 64) as u64,
            handle: identity.handle,
        }
    }
}

impl From<StoredIdentity> for FileIdentity {
    fn from(identity: StoredIdentity) -> Self {
        Self {
            ino: (u128::from(identity.ino_high) << 64) | u128::from(identity.ino),
            birth_ns: identity.birth_ns,
            handle: identity.handle,
        }
    }
}
