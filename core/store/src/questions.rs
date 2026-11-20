// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use koidrive_engine::guard::{Counted, GuardState, RemoteDeletion, Trash};
use koidrive_engine::question::{Answer, Frozen, Held, Limit, Question, QuestionKind, Revisions};
use koidrive_engine::state::GuardChanges;
use koidrive_engine::tree::{Content, NodeId, Shape};
use prost::Message;
use rusqlite::{Row, params};

use crate::engine::{kind, node_kind};
use crate::{Error, Result, Tx, decode};

type HeldBy = BTreeMap<String, BTreeMap<NodeId, Held>>;

impl Tx<'_> {
    pub(crate) fn open_questions(
        &self,
        questions: &mut Vec<Question>,
        abandoned: &mut dyn FnMut() -> bool,
    ) -> Result<bool> {
        let mut held = HeldBy::new();
        if !self.held(
            &mut held,
            "SELECT question_id, top_id, files FROM open_question_items",
            "SELECT question_id, top_id, node_id, content_rev, meta_rev FROM open_question_members",
            abandoned,
        )? {
            return Ok(false);
        }
        let mut shown: BTreeMap<String, BTreeMap<NodeId, Shape>> = BTreeMap::new();
        let mut stmt = self.0.prepare_cached(
            "SELECT question_id, node_id, parent_id, name, kind, content FROM open_question_shapes",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let shape = Shape {
                parent: NodeId::from(row.get::<_, String>(2)?),
                name: row.get(3)?,
                kind: kind(row.get(4)?)?,
                content: decode(row.get(5)?)?,
            };
            (shown.entry(row.get(0)?).or_default())
                .insert(NodeId::from(row.get::<_, String>(1)?), shape);
        }
        let mut frozen: BTreeMap<String, BTreeMap<NodeId, Frozen>> = BTreeMap::new();
        let mut stmt = self.0.prepare_cached(
            "SELECT question_id, node_id, parent_id, name, taken FROM open_question_frozen",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let node = Frozen {
                parent: NodeId::from(row.get::<_, String>(2)?),
                name: row.get(3)?,
                taken: row.get(4)?,
            };
            (frozen.entry(row.get(0)?).or_default())
                .insert(NodeId::from(row.get::<_, String>(1)?), node);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT question_id, asked_at_ms, kind, answer, frozen FROM open_questions
             ORDER BY position",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let id: String = row.get(0)?;
            let kind = serde_json::from_str::<QuestionKind>(&row.get::<_, String>(2)?).map_err(
                |error| {
                    Error::InvalidEngineState(format!(
                        "question {id:?} has an unknown kind: {error}"
                    ))
                },
            )?;
            let answer = (row.get::<_, Option<String>>(3)?)
                .map(|answer| answer.parse::<Answer>())
                .transpose()
                .map_err(Error::InvalidEngineState)?;
            let is_frozen: bool = row.get(4)?;
            questions.push(Question {
                held: held.remove(&id).unwrap_or_default(),
                shown: shown.remove(&id).unwrap_or_default(),
                frozen: is_frozen.then(|| frozen.remove(&id).unwrap_or_default()),
                id,
                asked_at_ms: row.get(1)?,
                kind,
                answer,
            });
        }
        if let Some(id) = held.keys().chain(shown.keys()).chain(frozen.keys()).next() {
            return Err(Error::InvalidEngineState(format!(
                "items held by an unknown question {id:?}"
            )));
        }
        Ok(true)
    }

    pub(crate) fn set_open_questions(&self, questions: &[Question]) -> Result<()> {
        for table in [
            "open_question_frozen",
            "open_question_shapes",
            "open_question_members",
            "open_question_items",
            "open_questions",
        ] {
            self.0.execute(&format!("DELETE FROM {table}"), [])?;
        }
        let mut stmt = self.0.prepare_cached(
            "INSERT INTO open_questions (question_id, position, asked_at_ms, kind, answer, frozen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut frozen = self.0.prepare_cached(
            "INSERT INTO open_question_frozen (question_id, node_id, parent_id, name, taken)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut items = self.0.prepare_cached(
            "INSERT INTO open_question_items (question_id, top_id, files) VALUES (?1, ?2, ?3)",
        )?;
        let mut members = self.0.prepare_cached(
            "INSERT INTO open_question_members (question_id, top_id, node_id, content_rev, meta_rev)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut shapes = self.0.prepare_cached(
            "INSERT INTO open_question_shapes (question_id, node_id, parent_id, name, kind, content)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (position, question) in questions.iter().enumerate() {
            let kind = serde_json::to_string(&question.kind).expect("a question kind serializes");
            stmt.execute(params![
                question.id,
                position as u64,
                question.asked_at_ms,
                kind,
                question.answer.map(Answer::as_str),
                question.frozen.is_some(),
            ])?;
            for (node, below) in question.frozen.iter().flatten() {
                frozen.execute(params![
                    question.id,
                    node.to_string(),
                    below.parent.to_string(),
                    below.name,
                    below.taken
                ])?;
            }
            for (top, item) in &question.held {
                items.execute(params![question.id, top.to_string(), item.files])?;
                for (node, revisions) in &item.members {
                    let (content_rev, meta_rev) = split(revisions);
                    members.execute(params![
                        question.id,
                        top.to_string(),
                        node.to_string(),
                        content_rev,
                        meta_rev
                    ])?;
                }
            }
            for (node, shape) in &question.shown {
                shapes.execute(params![
                    question.id,
                    node.to_string(),
                    shape.parent.to_string(),
                    shape.name,
                    node_kind(shape.kind),
                    shape.content.as_ref().map(Content::encode_to_vec),
                ])?;
            }
        }
        Ok(())
    }

    pub(crate) fn guard(
        &self,
        guard: &mut GuardState,
        abandoned: &mut dyn FnMut() -> bool,
    ) -> Result<bool> {
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, at_ms, files FROM guard_ledger")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let counted = Counted {
                at_ms: row.get(1)?,
                files: row.get(2)?,
            };
            guard
                .ledger
                .insert(NodeId::from(row.get::<_, String>(0)?), counted);
        }
        let mut confirmed = HeldBy::new();
        if !self.held(
            &mut confirmed,
            "SELECT '', top_id, files FROM guard_confirmed",
            "SELECT '', top_id, node_id, content_rev, meta_rev FROM guard_confirmed_members",
            abandoned,
        )? {
            return Ok(false);
        }
        guard.confirmed = confirmed.remove("").unwrap_or_default();
        let mut stmt = self
            .0
            .prepare_cached("SELECT limit_crossed FROM guard_confirmed_limits")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let name: String = row.get(0)?;
            let limit = serde_json::from_value::<Limit>(serde_json::Value::String(name))
                .map_err(|error| Error::InvalidEngineState(format!("unknown limit: {error}")))?;
            guard.confirmed_limits.insert(limit);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT top_id, known, trash_group, trashed_seq, at_ms, files FROM guard_remote",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let deletion = RemoteDeletion {
                top: NodeId::from(row.get::<_, String>(0)?),
                trash: trash(row.get(1)?, row.get(2)?, row.get(3)?),
            };
            let counted = Counted {
                at_ms: row.get(4)?,
                files: row.get(5)?,
            };
            guard.remote.insert(deletion, counted);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT top_id, known, trash_group, trashed_seq FROM guard_remote_dismissed",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            guard.dismissed.insert(RemoteDeletion {
                top: NodeId::from(row.get::<_, String>(0)?),
                trash: trash(row.get(1)?, row.get(2)?, row.get(3)?),
            });
        }
        Ok(true)
    }

    pub(crate) fn write_guard(&self, changes: &GuardChanges) -> Result<()> {
        let GuardChanges {
            ledger,
            confirmed,
            confirmed_limits,
            remote,
            dismissed,
        } = changes;
        self.write_node_map(
            ledger,
            "DELETE FROM guard_ledger WHERE node_id = ?1",
            "INSERT OR REPLACE INTO guard_ledger (node_id, at_ms, files) VALUES (?1, ?2, ?3)",
            |put, id, counted| put.execute(params![id, counted.at_ms, counted.files]),
        )?;
        let mut unlist = self
            .0
            .prepare_cached("DELETE FROM guard_confirmed_members WHERE top_id = ?1")?;
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM guard_confirmed WHERE top_id = ?1")?;
        let mut items = self
            .0
            .prepare_cached("INSERT INTO guard_confirmed (top_id, files) VALUES (?1, ?2)")?;
        let mut members = self.0.prepare_cached(
            "INSERT INTO guard_confirmed_members (top_id, node_id, content_rev, meta_rev)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (top, item) in confirmed {
            let top = top.to_string();
            unlist.execute([&top])?;
            remove.execute([&top])?;
            let Some(item) = item else {
                continue;
            };
            items.execute(params![top, item.files])?;
            for (node, revisions) in &item.members {
                let (content_rev, meta_rev) = split(revisions);
                members.execute(params![top, node.to_string(), content_rev, meta_rev])?;
            }
        }
        if let Some(limits) = confirmed_limits {
            self.0.execute("DELETE FROM guard_confirmed_limits", [])?;
            let mut stmt = self
                .0
                .prepare_cached("INSERT INTO guard_confirmed_limits (limit_crossed) VALUES (?1)")?;
            for limit in limits {
                stmt.execute(params![limit_name(*limit)])?;
            }
        }
        let mut remove = self.0.prepare_cached(
            "DELETE FROM guard_remote
             WHERE top_id = ?1 AND known = ?2 AND trash_group = ?3 AND trashed_seq = ?4",
        )?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO guard_remote
             (top_id, known, trash_group, trashed_seq, at_ms, files)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (deletion, counted) in remote {
            let (known, group, seq) = split_trash(&deletion.trash);
            let top = deletion.top.to_string();
            match counted {
                Some(counted) => put.execute(params![
                    top,
                    known,
                    group,
                    seq,
                    counted.at_ms,
                    counted.files
                ])?,
                None => remove.execute(params![top, known, group, seq])?,
            };
        }
        let mut remove = self.0.prepare_cached(
            "DELETE FROM guard_remote_dismissed
             WHERE top_id = ?1 AND known = ?2 AND trash_group = ?3 AND trashed_seq = ?4",
        )?;
        let mut put = self.0.prepare_cached(
            "INSERT OR IGNORE INTO guard_remote_dismissed (top_id, known, trash_group, trashed_seq)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (deletion, held) in dismissed {
            let (known, group, seq) = split_trash(&deletion.trash);
            match held {
                true => put.execute(params![deletion.top.to_string(), known, group, seq])?,
                false => remove.execute(params![deletion.top.to_string(), known, group, seq])?,
            };
        }
        Ok(())
    }

    fn held(
        &self,
        held: &mut HeldBy,
        items: &str,
        members: &str,
        abandoned: &mut dyn FnMut() -> bool,
    ) -> Result<bool> {
        let mut stmt = self.0.prepare_cached(items)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let item = Held {
                members: BTreeMap::new(),
                files: row.get(2)?,
            };
            held.entry(row.get(0)?)
                .or_default()
                .insert(NodeId::from(row.get::<_, String>(1)?), item);
        }
        let mut stmt = self.0.prepare_cached(members)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = next_row!(rows, abandoned) {
            let (owner, top): (String, NodeId) =
                (row.get(0)?, NodeId::from(row.get::<_, String>(1)?));
            let item = (held.get_mut(&owner))
                .and_then(|items| items.get_mut(&top))
                .ok_or_else(|| {
                    Error::InvalidEngineState(format!("a held member of an unknown item {top:?}"))
                })?;
            item.members
                .insert(NodeId::from(row.get::<_, String>(2)?), revisions(row)?);
        }
        Ok(true)
    }
}

fn limit_name(limit: Limit) -> String {
    match serde_json::to_value(limit) {
        Ok(serde_json::Value::String(name)) => name,
        _ => unreachable!("a limit serializes as its name"),
    }
}

fn split_trash(trash: &Trash) -> (bool, &str, u64) {
    match trash {
        Trash::Group { group, seq } => (true, group, *seq),
        Trash::Unknown { no_later_than } => (false, "", *no_later_than),
    }
}

fn trash(known: bool, group: String, seq: u64) -> Trash {
    match known {
        true => Trash::Group { group, seq },
        false => Trash::Unknown { no_later_than: seq },
    }
}

fn split(revisions: &Option<Revisions>) -> (Option<u64>, Option<u64>) {
    match revisions {
        Some(revisions) => (Some(revisions.content_rev), Some(revisions.meta_rev)),
        None => (None, None),
    }
}

fn revisions(row: &Row) -> Result<Option<Revisions>> {
    Ok(match (row.get(3)?, row.get(4)?) {
        (Some(content_rev), Some(meta_rev)) => Some(Revisions {
            content_rev,
            meta_rev,
        }),
        _ => None,
    })
}
