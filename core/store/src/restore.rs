// SPDX-License-Identifier: Apache-2.0

//! The restores a person asked for (design §6.3) and the two tables that hold them. Each table
//! has one writer. `restore_requests` is the inbox: the driver's API inserts a request there
//! from its own connection, and the task that runs the engine takes it. `restore_intents` is
//! engine state, the queued restores and the outcomes of [`CommitsState`], and only
//! [`Tx::set_engine_state`] writes it. A request moves from one to the other in one transaction,
//! so it is never in both and never in neither.
//!
//! [`CommitsState`]: koidrive_engine::state::CommitsState

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::restore::{Outcome, Rejection, Resolved, RestoreId, RestoreIntent};
use koidrive_engine::tree::NodeId;
use koidrive_proto::v1::Restore;
use rusqlite::params;

use crate::{Error, Result, Tx};

/// What a person saw when they asked for a restore: the item's path in the space then, and for a
/// version restore when that version was saved. A restore of a node the folder's copy of the
/// server's tree never held still shows where it was.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AskedRestore {
    pub path: Option<String>,
    pub version_saved_at_ms: Option<i64>,
}

/// The four columns both tables keep an intent in.
const INTENT: &str = "node_id, version_id, target_parent_id, name";

impl Tx<'_> {
    /// Records a restore a person asked for, for the engine to take with
    /// [`Tx::take_restore_requests`]. It is durable once the transaction commits, whether or not
    /// anything runs the engine.
    pub fn request_restore(&self, id: &str, intent: &RestoreIntent) -> Result<()> {
        self.0.execute(
            &format!(
                "INSERT INTO restore_requests (restore_id, {INTENT}) VALUES (?1, ?2, ?3, ?4, ?5)"
            ),
            params![
                id,
                intent.node(),
                intent.version_id(),
                intent.parent(),
                intent.name()
            ],
        )?;
        Ok(())
    }

    /// Records what the person saw when they asked for the restore `id`, for
    /// [`Tx::asked_restores`], and forgets what it recorded for restores the store no longer holds.
    pub fn note_asked_restore(&self, id: &str, asked: &AskedRestore) -> Result<()> {
        self.0.execute(
            "DELETE FROM restore_asked WHERE restore_id NOT IN
             (SELECT restore_id FROM restore_requests UNION SELECT restore_id FROM restore_intents)",
            [],
        )?;
        self.0.execute(
            "INSERT OR REPLACE INTO restore_asked (restore_id, path, version_saved_at_ms)
             VALUES (?1, ?2, ?3)",
            params![id, asked.path, asked.version_saved_at_ms],
        )?;
        Ok(())
    }

    /// What [`Tx::note_asked_restore`] recorded, by restore ID.
    pub fn asked_restores(&self) -> Result<BTreeMap<RestoreId, AskedRestore>> {
        let mut stmt = self
            .0
            .prepare_cached("SELECT restore_id, path, version_saved_at_ms FROM restore_asked")?;
        let rows = stmt.query_map([], |row| {
            let asked = AskedRestore {
                path: row.get(1)?,
                version_saved_at_ms: row.get(2)?,
            };
            Ok((row.get(0)?, asked))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The requests the engine has not taken yet, in request order.
    pub fn restore_requests(&self) -> Result<Vec<(RestoreId, RestoreIntent)>> {
        let mut stmt = self.0.prepare_cached(&format!(
            "SELECT restore_id, {INTENT} FROM restore_requests ORDER BY restore_id"
        ))?;
        let mut rows = stmt.query([])?;
        let mut requests = Vec::new();
        while let Some(row) = rows.next()? {
            requests.push((row.get(0)?, intent(row)?));
        }
        Ok(requests)
    }

    /// Returns the waiting requests in request order and removes them. The caller queues them in
    /// the engine and saves the engine state in this transaction.
    pub fn take_restore_requests(&self) -> Result<Vec<(RestoreId, RestoreIntent)>> {
        let requests = self.restore_requests()?;
        self.0.execute("DELETE FROM restore_requests", [])?;
        Ok(requests)
    }

    /// The nodes the engine holds as problems. A queued restore of one waits until the problem
    /// is cleared ([`RestoreIntent::held_by`]).
    pub fn problem_nodes(&self) -> Result<BTreeSet<NodeId>> {
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id FROM node_sets WHERE node_set = 'problems'")?;
        let nodes = stmt.query_map([], |row| row.get(0))?;
        Ok(nodes.collect::<rusqlite::Result<_>>()?)
    }

    /// The restores the engine holds: those without an outcome in request order, and those that
    /// ended, oldest first.
    pub fn restores(&self) -> Result<(BTreeMap<RestoreId, RestoreIntent>, Vec<Resolved>)> {
        let mut stmt = self.0.prepare_cached(&format!(
            "SELECT restore_id, {INTENT}, outcome, outcome_node_id, outcome_name, reject_reason
             FROM restore_intents ORDER BY position, restore_id"
        ))?;
        let mut rows = stmt.query([])?;
        let (mut queued, mut resolved) = (BTreeMap::new(), Vec::new());
        while let Some(row) = rows.next()? {
            let id: RestoreId = row.get(0)?;
            let intent = intent(row)?;
            let text = |column: usize| Ok::<_, Error>(row.get::<_, Option<String>>(column)?);
            let outcome = match text(5)?.as_deref() {
                None => {
                    queued.insert(id, intent);
                    continue;
                }
                Some("restored") => Outcome::Restored,
                Some("renamed") => Outcome::Renamed {
                    name: text(7)?.unwrap_or_default(),
                },
                Some("copied") => Outcome::Copied {
                    node: text(6)?.unwrap_or_default(),
                    name: text(7)?.unwrap_or_default(),
                },
                Some("rejected") => {
                    Outcome::Rejected(Rejection::from_reason(&text(8)?.unwrap_or_default()))
                }
                Some("does_not_fit") => Outcome::DoesNotFit,
                Some(other) => {
                    return Err(Error::InvalidEngineState(format!(
                        "restore outcome {other:?}"
                    )));
                }
            };
            resolved.push(Resolved {
                id,
                intent,
                outcome,
            });
        }
        Ok((queued, resolved))
    }

    /// Writes the engine's restores into the emptied `restore_intents`.
    pub(crate) fn set_restores(
        &self,
        queued: &BTreeMap<RestoreId, RestoreIntent>,
        resolved: &[Resolved],
    ) -> Result<()> {
        let mut stmt = self.0.prepare_cached(&format!(
            "INSERT INTO restore_intents
             (restore_id, {INTENT}, position, outcome, outcome_node_id, outcome_name, reject_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
        ))?;
        let none = (None::<usize>, None, None, None, None);
        let ended = resolved.iter().enumerate().map(|(position, ended)| {
            let (outcome, node, name, reason) = match &ended.outcome {
                Outcome::Restored => ("restored", None, None, None),
                Outcome::Renamed { name } => ("renamed", None, Some(name.as_str()), None),
                Outcome::Copied { node, name } => {
                    ("copied", Some(node.as_str()), Some(name.as_str()), None)
                }
                Outcome::Rejected(rejection) => ("rejected", None, None, Some(rejection.reason())),
                Outcome::DoesNotFit => ("does_not_fit", None, None, None),
            };
            let outcome = (Some(position), Some(outcome), node, name, reason);
            (&ended.id, &ended.intent, outcome)
        });
        let queued = queued.iter().map(|(id, intent)| (id, intent, none));
        for (id, intent, (position, outcome, node, name, reason)) in queued.chain(ended) {
            stmt.execute(params![
                id,
                intent.node(),
                intent.version_id(),
                intent.parent(),
                intent.name(),
                position,
                outcome,
                node,
                name,
                reason,
            ])?;
        }
        Ok(())
    }
}

/// The intent in columns 1 to 4 of `row`. A row that is not a restore the server would take is
/// refused, as any other state the engine could not have written.
fn intent(row: &rusqlite::Row<'_>) -> Result<RestoreIntent> {
    let text = |column: usize| Ok::<_, Error>(row.get::<_, Option<String>>(column)?);
    let op = Restore {
        node_id: row.get(1)?,
        version_id: text(2)?.unwrap_or_default(),
        target_parent_id: text(3)?.unwrap_or_default(),
        name: text(4)?.unwrap_or_default(),
    };
    RestoreIntent::from_op(op).map_err(|malformed| {
        Error::InvalidEngineState(format!("a stored restore has a malformed {}", malformed.0))
    })
}
