// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use koidrive_engine::journal::{
    Absent, Author, Field, Flight, HistoryEntry, IntentId, JournalChanges, JournalState, Row,
    RowState, Seen,
};
use koidrive_engine::tree::NodeId;
use koidrive_proto::v1::FieldIntents;
use prost::Message;
use rusqlite::{OptionalExtension, params};

use crate::{Error, Result, Tx, decode};

fn invalid(what: String) -> Error {
    Error::InvalidEngineState(what)
}

fn field(text: &str) -> Result<Field> {
    Field::parse(text).ok_or_else(|| invalid(format!("journal field {text:?}")))
}

fn intent(bytes: &[u8]) -> Result<IntentId> {
    IntentId::from_bytes(bytes)
        .ok_or_else(|| invalid(format!("an intent id of {} bytes", bytes.len())))
}

fn json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|error| invalid(format!("journal value: {error}")))
}

fn parse<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|error| invalid(format!("journal value: {error}")))
}

impl Tx<'_> {
    pub(crate) fn journal_state(&self) -> Result<JournalState> {
        let mut state = JournalState::default();
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, field, intent, author, base, value, wire_base, state, flight, held_as,
                    held_version_id, time_ms, replay
             FROM journal",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let node = NodeId::from(row.get::<_, String>(0)?);
            let field = field(&row.get::<_, String>(1)?)?;
            let state_name: String = row.get(7)?;
            let row_state = match state_name.as_str() {
                "local" => RowState::Local {
                    flight: (row.get::<_, Option<String>>(8)?)
                        .map(|text| parse::<Flight>(&text))
                        .transpose()?,
                },
                "sent" => RowState::Sent,
                "held" => RowState::Held {
                    held_as: intent(&row.get::<_, Vec<u8>>(9)?)?,
                    version_id: row.get(10)?,
                },
                other => return Err(invalid(format!("journal state {other:?}"))),
            };
            let journal_row = Row {
                intent: intent(&row.get::<_, Vec<u8>>(2)?)?,
                author: match row.get::<_, Option<String>>(3)? {
                    None => Author::Own,
                    Some(author) => {
                        Author::Carried(Some(author).filter(|author| !author.is_empty()))
                    }
                },
                base: (row.get::<_, Option<Vec<u8>>>(4)?)
                    .map(|bytes| intent(&bytes))
                    .transpose()?,
                value: parse(&row.get::<_, String>(5)?)?,
                wire_base: parse(&row.get::<_, String>(6)?)?,
                state: row_state,
                time_ms: row.get(11)?,
                replay: row.get(12)?,
            };
            if journal_row.value.field() != field {
                return Err(invalid(format!(
                    "journal row {node} {} holds another field",
                    field.as_str()
                )));
            }
            state.rows.insert((node, field), journal_row);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, field, intent, applied_at_ms, in_local FROM intent_history ORDER BY node_id, field, ord",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let key = (
                NodeId::from(row.get::<_, String>(0)?),
                field(&row.get::<_, String>(1)?)?,
            );
            state.history.entry(key).or_default().push(HistoryEntry {
                intent: intent(&row.get::<_, Vec<u8>>(2)?)?,
                applied_at_ms: row.get(3)?,
                in_local: row.get(4)?,
            });
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, view_node_id, field, intent, value FROM delete_views ORDER BY node_id, ord",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let node = NodeId::from(row.get::<_, String>(0)?);
            state.seen.entry(node).or_default().push(Seen {
                node: NodeId::from(row.get::<_, String>(1)?),
                field: field(&row.get::<_, String>(2)?)?,
                intent: intent(&row.get::<_, Vec<u8>>(3)?)?,
                value: (row.get::<_, Option<String>>(4)?)
                    .map(|text| parse(&text))
                    .transpose()?,
            });
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, since_ms, purged FROM journal_absent")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let absent = Absent {
                since_ms: row.get(1)?,
                purged: row.get(2)?,
            };
            state
                .absent
                .insert(NodeId::from(row.get::<_, String>(0)?), absent);
        }
        if let Some(listed) = self
            .0
            .query_row("SELECT listed FROM journal_state WHERE id = 1", [], |row| {
                row.get::<_, bool>(0)
            })
            .optional()?
        {
            state.listed = listed;
        }
        Ok(state)
    }

    pub(crate) fn write_journal_changes(&self, changes: &JournalChanges) -> Result<()> {
        let JournalChanges {
            rows,
            history,
            seen,
            listed,
            absent,
        } = changes;
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM journal WHERE node_id = ?1 AND field = ?2")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO journal (node_id, field, intent, author, base, value, wire_base, state, flight,
                                             held_as, held_version_id, time_ms, replay)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )?;
        for ((node, field), row) in rows {
            let node = node.to_string();
            let Some(row) = row else {
                remove.execute(params![node, field.as_str()])?;
                continue;
            };
            let (state, flight, held_as, version_id) = match &row.state {
                RowState::Local { flight } => {
                    ("local", flight.as_ref().map(json).transpose()?, None, None)
                }
                RowState::Sent => ("sent", None, None, None),
                RowState::Held {
                    held_as,
                    version_id,
                } => (
                    "held",
                    None,
                    Some(held_as.as_bytes().to_vec()),
                    version_id.clone(),
                ),
            };
            put.execute(params![
                node,
                field.as_str(),
                row.intent.as_bytes().to_vec(),
                match &row.author {
                    Author::Own => None,
                    Author::Carried(author) => Some(author.as_deref().unwrap_or_default()),
                },
                row.base.map(|base| base.as_bytes().to_vec()),
                json(&row.value)?,
                json(&row.wire_base)?,
                state,
                flight,
                held_as,
                version_id,
                row.time_ms,
                row.replay,
            ])?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM intent_history WHERE node_id = ?1 AND field = ?2")?;
        let mut put = self.0.prepare_cached(
            "INSERT INTO intent_history (node_id, field, ord, intent, applied_at_ms, in_local)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for ((node, field), entries) in history {
            let node = node.to_string();
            remove.execute(params![node, field.as_str()])?;
            for (ord, entry) in entries.iter().flatten().enumerate() {
                put.execute(params![
                    node,
                    field.as_str(),
                    ord,
                    entry.intent.as_bytes().to_vec(),
                    entry.applied_at_ms,
                    entry.in_local,
                ])?;
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM delete_views WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT INTO delete_views (node_id, ord, view_node_id, field, intent, value)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (node, entries) in seen {
            let node = node.to_string();
            remove.execute([&node])?;
            for (ord, entry) in entries.iter().flatten().enumerate() {
                put.execute(params![
                    node,
                    ord,
                    entry.node.to_string(),
                    entry.field.as_str(),
                    entry.intent.as_bytes().to_vec(),
                    entry.value.as_ref().map(json).transpose()?,
                ])?;
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM journal_absent WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT INTO journal_absent (node_id, since_ms, purged) VALUES (?1, ?2, ?3)
             ON CONFLICT (node_id) DO UPDATE SET since_ms = excluded.since_ms, purged = excluded.purged",
        )?;
        for (node, absent) in absent {
            let node = node.to_string();
            match absent {
                Some(absent) => put.execute(params![node, absent.since_ms, absent.purged])?,
                None => remove.execute([node])?,
            };
        }
        if let Some(listed) = listed {
            self.0.execute(
                "INSERT INTO journal_state (id, listed, backfill_owed) VALUES (1, ?1, 1)
                 ON CONFLICT (id) DO UPDATE SET listed = excluded.listed",
                [listed],
            )?;
        }
        Ok(())
    }

    /// Whether the store still owes the journal its backfill from the tables of earlier releases (RFC 0029 §7.1).
    pub fn journal_backfill_owed(&self) -> Result<bool> {
        Ok((self.0)
            .query_row(
                "SELECT backfill_owed FROM journal_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(true))
    }

    pub fn journal_backfilled(&self) -> Result<()> {
        self.0.execute(
            "INSERT INTO journal_state (id, listed, backfill_owed) VALUES (1, 0, 0)
             ON CONFLICT (id) DO UPDATE SET backfill_owed = 0",
            [],
        )?;
        Ok(())
    }

    /// Sets the intents of R's node, which the engine keeps when the server sends none.
    pub fn set_remote_intents(&self, node_id: &str, intents: Option<&FieldIntents>) -> Result<()> {
        self.0.execute(
            "UPDATE remote_nodes SET intents = ?2 WHERE node_id = ?1",
            params![node_id, intents.map(FieldIntents::encode_to_vec)],
        )?;
        Ok(())
    }

    pub fn set_synced_intents(&self, node_id: &str, intents: Option<&FieldIntents>) -> Result<()> {
        self.0.execute(
            "UPDATE synced_nodes SET intents = ?2 WHERE node_id = ?1",
            params![node_id, intents.map(FieldIntents::encode_to_vec)],
        )?;
        Ok(())
    }

    /// The intents of S's nodes, by node.
    pub fn synced_intents(&self) -> Result<BTreeMap<NodeId, FieldIntents>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, intents FROM synced_nodes WHERE intents IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                NodeId::from(row.get::<_, String>(0)?),
                row.get::<_, Vec<u8>>(1)?,
            ))
        })?;
        rows.map(|row| {
            let (node, bytes) = row?;
            Ok((
                node,
                decode::<FieldIntents>(Some(bytes))?.unwrap_or_default(),
            ))
        })
        .collect()
    }
}
