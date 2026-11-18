// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use koidrive_proto::v1::{self as proto, change_record, op, op_result};

use crate::journal::{
    Author, Field, FieldIntent, HeldRow, HistoryEntry, IntentId, Journal, NodeIntents,
    OpsWithoutIntent, Place, RemoteField, Row, RowKey, RowState, Seen, Value, Wire, WireBase,
    delete_view,
};
use crate::names::nfc;
use crate::state::ContentValue;
use crate::tree::{Kind, LocalNode, NodeId, RemoteNode, Shape, Tree};

use super::Trees;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Compact {
    AfterChange,
    Loaded,
}

pub(super) enum Carrier {
    LostCreate,
    UnsentCreate,
    Resend,
}

fn place(shape: &Shape, mark: Option<&str>) -> Value {
    Value::Place(Place {
        parent: shape.parent.clone(),
        name: shape.name.clone(),
        mark: mark.map(str::to_owned),
    })
}

fn content(shape: &Shape, version_id: Option<&str>) -> Option<Value> {
    (shape.content.as_ref()).map(|content| Value::Content {
        version_id: version_id.map(str::to_owned),
        value: ContentValue::from(content),
    })
}

fn remote_value(node: &RemoteNode, field: Field) -> Option<Value> {
    match field {
        Field::Place => Some(place(&node.shape, node.pending_for.as_deref())),
        Field::Content => content(
            &node.shape,
            Some(node.version_id.as_str()).filter(|id| !id.is_empty()),
        ),
        Field::Status => Some(Value::Status {
            live: !node.trashed,
        }),
    }
}

fn remote_intent_of(node: &RemoteNode, field: Field) -> Option<&FieldIntent> {
    node.intents.as_ref()?.get(field)
}

pub(super) fn full_held(r: &Tree<RemoteNode>, (node, field): &RowKey, held: &HeldRow) -> Row {
    let remote = r.get(node).expect("R holds the node of every compact row");
    let value = remote_value(remote, *field).expect("R holds the value of every compact row");
    let time_ms = remote_intent_of(remote, *field).map_or(0, |intent| intent.applied_at_ms);
    let version_id = (*field == Field::Content && !remote.version_id.is_empty())
        .then(|| remote.version_id.clone());
    held.full(value, time_ms, version_id)
}

fn named_place(parent: &str, name: &str, mark: &str) -> Value {
    Value::Place(Place {
        parent: parent.to_owned(),
        name: name.to_owned(),
        mark: Some(mark.to_owned()).filter(|mark| !mark.is_empty()),
    })
}

fn sent_content(content: Option<&proto::Content>) -> Option<Value> {
    content.map(|content| Value::Content {
        version_id: None,
        value: ContentValue::from(content),
    })
}

fn op_fields(op: &op::Op) -> (&str, Vec<(Field, Option<Value>)>, bool) {
    let live = |live| Some(Value::Status { live });
    match op {
        op::Op::CreateDir(create) => (
            &create.node_id,
            vec![
                (
                    Field::Place,
                    Some(named_place(
                        &create.parent_id,
                        &create.name,
                        &create.pending_for,
                    )),
                ),
                (Field::Status, live(true)),
            ],
            false,
        ),
        op::Op::CreateFile(create) => (
            &create.node_id,
            vec![
                (
                    Field::Place,
                    Some(named_place(
                        &create.parent_id,
                        &create.name,
                        &create.pending_for,
                    )),
                ),
                (Field::Content, sent_content(create.content.as_ref())),
                (Field::Status, live(true)),
            ],
            !create.source_version_id.is_empty(),
        ),
        op::Op::UpdateContent(update) => (
            &update.node_id,
            vec![(Field::Content, sent_content(update.content.as_ref()))],
            update.resend,
        ),
        op::Op::Move(moved) => (
            &moved.node_id,
            vec![(
                Field::Place,
                Some(named_place(
                    &moved.new_parent_id,
                    &moved.new_name,
                    &moved.pending_for,
                )),
            )],
            moved.from.is_some(),
        ),
        op::Op::Delete(delete) => (&delete.node_id, vec![(Field::Status, live(false))], false),
        op::Op::Restore(restore) if restore.version_id.is_empty() => {
            (&restore.node_id, vec![(Field::Status, live(true))], false)
        }
        op::Op::Restore(restore) => (&restore.node_id, vec![(Field::Content, None)], false),
        op::Op::MarkGitTracked(mark) => (&mark.node_id, Vec::new(), false),
    }
}

fn wire_intent(row: &Row, device_id: &str) -> proto::Intent {
    proto::Intent {
        id: row.intent.as_bytes().to_vec(),
        author_device_id: match &row.author {
            Author::Own => device_id.to_owned(),
            Author::Carried(author) => author.clone().unwrap_or_default(),
        },
        replay: row.replay,
        over: Vec::new(),
        applied_at_ms: if row.replay { row.time_ms } else { 0 },
    }
}

fn same(a: Option<&Value>, b: Option<&Value>) -> bool {
    a.zip(b).is_some_and(|(a, b)| a.same(b))
}

impl Trees {
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    pub fn journal_mut(&mut self) -> &mut Journal {
        &mut self.journal
    }

    pub fn journal_row(&self, node: &str, field: Field) -> Option<Cow<'_, Row>> {
        if let Some(row) = self.journal.row(node, field) {
            return Some(Cow::Borrowed(row));
        }
        let held = self.journal.compact_row(node, field)?;
        Some(Cow::Owned(full_held(
            &self.r,
            &(node.to_owned(), field),
            held,
        )))
    }

    /// Every journal row whole, as [`Trees::journal_row`] gives it.
    pub fn journal_rows(&self) -> impl Iterator<Item = (&RowKey, Cow<'_, Row>)> {
        let compact = (self.journal.compact_rows())
            .map(|(key, held)| (key, Cow::Owned(full_held(&self.r, key, held))));
        (self.journal.rows())
            .map(|(key, row)| (key, Cow::Borrowed(row)))
            .chain(compact)
    }

    pub(super) fn compact_journal(&mut self, node: &str, when: Compact) {
        let Some(remote) = self.r.get(node) else {
            return;
        };
        for field in Field::ALL {
            let Some(intent) = remote_intent_of(remote, field) else {
                continue;
            };
            let implied = HistoryEntry {
                intent: intent.id,
                applied_at_ms: intent.applied_at_ms,
                in_local: self.local_holds_remote(node, remote, field),
            };
            if when == Compact::AfterChange {
                self.journal.trim_history(node, field, &implied);
            }
            let key = (node.to_owned(), field);
            let compact = (self.journal.row(node, field))
                .and_then(|row| Some((row, HeldRow::of(row)?)))
                .is_some_and(|(row, held)| full_held(&self.r, &key, &held) == *row);
            if compact {
                self.journal.compact(key);
            }
        }
    }

    pub(super) fn write_out_journal(&mut self, node: &str) {
        for field in Field::ALL {
            let key = (node.to_owned(), field);
            if let Some(held) = self.journal.compact_row(node, field) {
                let row = full_held(&self.r, &key, held);
                self.journal.write_out(key, row);
            }
        }
    }

    pub(super) fn journal_lost(&mut self) {
        let nodes: Vec<NodeId> = (self.journal.compact_rows())
            .map(|((node, _), _)| node.clone())
            .collect();
        for node in nodes {
            self.write_out_journal(&node);
        }
        self.journal.lose();
    }

    /// Fills the journal of a store from an earlier release out of its legacy tables, in the order of RFC 0029 §7.1;
    /// a later step never replaces a row an earlier one wrote. `in_flight` is the commit in flight and `lost` whether
    /// the store records a loss.
    pub fn backfill(&mut self, in_flight: Option<&proto::Commit>, lost: bool) {
        let row = |intent, author, base, value, state, replay| Row {
            intent,
            author,
            base,
            value,
            wire_base: WireBase::None,
            state,
            time_ms: 0,
            replay,
        };
        let carriers: BTreeSet<NodeId> = (self.lost_creates.iter())
            .chain(self.unsent_creates.iter())
            .chain(self.resends.iter())
            .cloned()
            .collect();
        for id in carriers {
            let replay = self.lost_creates.contains(&id) || self.resends.contains(&id);
            for (field, value) in self.local_values(&id) {
                if self.remote_intent(&id, field, &value).is_some() {
                    continue;
                }
                let intent = self.legacy_intent(&id, field, &value);
                let carried = Author::Carried(None);
                let local = RowState::Local { flight: None };
                (self.journal)
                    .insert_if_absent(&id, row(intent, carried, None, value, local, replay));
            }
        }
        for op in in_flight.into_iter().flat_map(|commit| &commit.ops) {
            let Some(kind) = &op.op else {
                continue;
            };
            let (node, fields, carried) = op_fields(kind);
            let own = !carried
                && (self.local_changes.contains(node)
                    || self.edited_here.contains_key(node)
                    || self.moved_here.contains_key(node));
            for (field, value) in fields {
                let Some(value) = value else {
                    continue;
                };
                let author = match own {
                    true => Author::Own,
                    false => Author::Carried(None),
                };
                let base = self.acknowledged_intent(node, field);
                let intent = self.journal.mint();
                (self.journal)
                    .insert_if_absent(node, row(intent, author, base, value, RowState::Sent, lost));
            }
        }
        let unsent_edits: Vec<NodeId> = self.unsent_edits.iter().cloned().collect();
        let changed: Vec<NodeId> = (self.l.iter().map(|(id, _)| id))
            .chain(self.s.iter().map(|(id, _)| id))
            .chain(self.waiting.keys())
            .filter(|id| !unsent_edits.contains(*id))
            .cloned()
            .collect::<BTreeSet<NodeId>>()
            .into_iter()
            .collect();
        for id in unsent_edits.into_iter().chain(changed) {
            let Some(acknowledged) = self.acknowledged(&id).cloned() else {
                continue;
            };
            if self.l.get(&id).is_none() {
                if self.journal_row(&id, Field::Status).is_none() {
                    let seen_list = self.seen_list(&id);
                    let base = self.acknowledged_intent(&id, Field::Status);
                    self.journal.user_delete(&id, seen_list, base);
                }
                continue;
            }
            for (field, value) in self.local_values(&id) {
                let seen = match field {
                    Field::Place => Some(place(&acknowledged, self.synced_mark(&id))),
                    Field::Content => content(&acknowledged, None),
                    Field::Status => Some(Value::Status { live: true }),
                };
                let from_flight = self
                    .journal_row(&id, field)
                    .map(|row| row.state == RowState::Sent);
                if same(seen.as_ref(), Some(&value)) || from_flight == Some(false) {
                    continue;
                }
                let base = self.acknowledged_intent(&id, field);
                self.journal.user_change(&id, value, WireBase::None, base);
            }
        }
        let held: Vec<(NodeId, Field, Option<IntentId>)> = (self.edited_here.keys())
            .map(|id| {
                let base = (self.replaced_versions.get(id))
                    .and_then(|versions| versions.last())
                    .map(|version| IntentId::legacy_content(id, version));
                (id.clone(), Field::Content, base)
            })
            .chain(self.moved_here.keys().map(|id| {
                let base = (self.past_positions.get(id))
                    .and_then(|positions| positions.last())
                    .map(|at| IntentId::legacy_place(id, at.rev, &at.parent, &at.name));
                (id.clone(), Field::Place, base)
            }))
            .chain(self.trashed_here.keys().map(|id| {
                let rev = self.r.get(id).map_or(0, |remote| remote.meta_rev);
                (
                    id.clone(),
                    Field::Status,
                    Some(IntentId::legacy_live(id, rev)),
                )
            }))
            .collect();
        for (id, field, base) in held {
            let Some(remote) = self.r.get(&id) else {
                continue;
            };
            let (Some(value), Some(intent)) =
                (remote_value(remote, field), remote_intent_of(remote, field))
            else {
                continue;
            };
            let version_id = (field == Field::Content && !remote.version_id.is_empty())
                .then(|| remote.version_id.clone());
            let state = RowState::Held {
                held_as: intent.id,
                version_id,
            };
            let mut held = row(intent.id, Author::Own, base, value, state, lost);
            held.time_ms = intent.applied_at_ms;
            self.journal.insert_if_absent(&id, held);
        }
        let past: Vec<(NodeId, Field, IntentId)> = (self.replaced_versions.iter())
            .flat_map(|(id, versions)| {
                (versions.iter()).map(|version| {
                    (
                        id.clone(),
                        Field::Content,
                        IntentId::legacy_content(id, version),
                    )
                })
            })
            .chain(self.past_positions.iter().flat_map(|(id, positions)| {
                (positions.iter()).map(|at| {
                    (
                        id.clone(),
                        Field::Place,
                        IntentId::legacy_place(id, at.rev, &at.parent, &at.name),
                    )
                })
            }))
            .collect();
        for (id, field, intent) in past {
            self.journal
                .remote_holds(&id, field, &FieldIntent::legacy(intent), false);
        }
        let nodes: Vec<NodeId> = self.r.iter().map(|(id, _)| id.clone()).collect();
        for id in nodes {
            self.compact_journal(&id, Compact::AfterChange);
        }
    }

    fn local_values(&self, id: &str) -> Vec<(Field, Value)> {
        let Some(local) = self.l.get(id) else {
            return Vec::new();
        };
        (Field::ALL.into_iter())
            .filter_map(|field| Some((field, self.local_value(id, &local.shape, field)?)))
            .collect()
    }

    fn acknowledged_intent(&self, id: &str, field: Field) -> Option<IntentId> {
        let shape = self.acknowledged(id)?;
        let value = match field {
            Field::Place => place(shape, self.synced_mark(id)),
            Field::Content => content(shape, None)?,
            Field::Status => Value::Status { live: true },
        };
        Some(self.legacy_intent(id, field, &value))
    }

    fn legacy_intent(&self, id: &str, field: Field, value: &Value) -> IntentId {
        if let Some(intent) = self.remote_intent(id, field, value) {
            return intent;
        }
        let remote = self.r.get(id);
        let rev = remote.map_or(0, |remote| remote.meta_rev);
        match value {
            Value::Place(place) => IntentId::legacy_place(id, rev, &place.parent, &place.name),
            Value::Content { version_id, .. } => {
                let version = (version_id.clone())
                    .or_else(|| {
                        Some(
                            self.content_provenance
                                .get(id, &self.r)?
                                .local
                                .as_ref()?
                                .version_id
                                .clone(),
                        )
                    })
                    .unwrap_or_default();
                IntentId::legacy_content(id, &version)
            }
            Value::Status { live: true } => IntentId::legacy_live(id, rev),
            Value::Status { live: false } => IntentId::legacy_trashed(
                id,
                remote.map_or("", |remote| remote.trash_group.as_str()),
            ),
        }
    }

    pub fn take_listed_intents(&mut self, listed: &Tree<RemoteNode>) {
        let mut ids = BTreeMap::new();
        for (id, node) in listed.iter() {
            let (Some(old), Some(intents)) = (self.r.get(id), node.intents.as_ref()) else {
                continue;
            };
            for field in Field::ALL {
                let (Some(server), Some(computed)) =
                    (intents.get(field), remote_intent_of(old, field))
                else {
                    continue;
                };
                if same(
                    remote_value(old, field).as_ref(),
                    remote_value(node, field).as_ref(),
                ) {
                    ids.insert(computed.id, server.id);
                }
            }
        }
        ids.retain(|computed, server| computed != server);
        let replaced = |intents: &NodeIntents| {
            let mut intents = intents.clone();
            for field in Field::ALL {
                if let Some(intent) = intents.get(field).cloned() {
                    let id = ids.get(&intent.id).copied().unwrap_or(intent.id);
                    intents.set(field, Some(FieldIntent { id, ..intent }));
                }
            }
            intents
        };
        let remote: Vec<(NodeId, RemoteNode)> = (self.r.iter())
            .filter_map(|(id, node)| {
                let intents = replaced(node.intents.as_ref()?);
                (Some(&intents) != node.intents.as_ref()).then(|| {
                    let mut node = node.clone();
                    node.intents = Some(intents);
                    (id.clone(), node)
                })
            })
            .collect();
        for (id, node) in remote {
            self.r.insert(id, node);
        }
        let synced: Vec<(NodeId, NodeIntents)> = (self.synced_intents.iter())
            .map(|(id, intents)| (id.clone(), replaced(intents)))
            .collect();
        for (id, intents) in synced {
            self.put_synced_intents(&id, Some(intents));
        }
        self.journal.replace_ids(&ids);
        self.journal.set_listed(true);
    }

    pub(super) fn end_carrier(&mut self, carrier: Carrier, id: &str) -> bool {
        let ended = match carrier {
            Carrier::LostCreate => self.lost_creates.remove(id),
            Carrier::UnsentCreate => self.unsent_creates.remove(id),
            Carrier::Resend => self.resends.remove(id),
        };
        if ended {
            self.ended_carriers.insert(id.to_owned());
        }
        ended
    }

    pub(super) fn end_carried_rows(&mut self) {
        for id in std::mem::take(&mut self.ended_carriers) {
            let carried = self.lost_creates.contains(&id)
                || self.unsent_creates.contains(&id)
                || self.resends.contains(&id);
            if !carried {
                self.journal.end_unsent_carried(&id);
            }
        }
    }

    /// The intents S holds for the node: for each field, the intent of the value S took.
    pub fn synced_intents(&self, id: &str) -> Option<NodeIntents> {
        self.s.get(id)?;
        Some(match self.synced_intents.get(id) {
            Some(intents) => intents.clone(),
            None => self.synced_from_remote(id),
        })
    }

    /// The intents S holds for the node where they differ from the intents of the values R holds, which the store
    /// keeps; elsewhere S follows R.
    pub fn synced_intents_apart_from_remote(&self, id: &str) -> Option<&NodeIntents> {
        self.synced_intents.get(id)
    }

    /// Sets the intents S holds for a node S holds, as the store kept them.
    pub fn set_synced_intents(&mut self, id: &str, intents: NodeIntents) {
        if self.s.get(id).is_some() {
            self.keep_synced_intents(id, intents);
        }
    }

    fn synced_from_remote(&self, id: &str) -> NodeIntents {
        let mut intents = NodeIntents::default();
        let Some(remote) = self.r.get(id) else {
            return intents;
        };
        for field in Field::ALL {
            if same(
                remote_value(remote, field).as_ref(),
                self.synced_value(id, field).as_ref(),
            ) {
                intents.set(field, remote_intent_of(remote, field).cloned());
            }
        }
        intents
    }

    fn keep_synced_intents(&mut self, id: &str, intents: NodeIntents) {
        let kept = (intents != self.synced_from_remote(id)).then_some(intents);
        self.put_synced_intents(id, kept);
    }

    fn put_synced_intents(&mut self, id: &str, intents: Option<NodeIntents>) {
        let before = match intents {
            Some(intents) => self.synced_intents.insert(id.to_owned(), intents),
            None => self.synced_intents.remove(id),
        };
        if before.as_ref() != self.synced_intents.get(id) {
            self.s.touch(id);
        }
    }

    pub(super) fn settle_synced_intents(&mut self, id: &str) {
        if let Some(intents) = self.synced_intents.get(id).cloned() {
            self.keep_synced_intents(id, intents);
        }
    }

    pub fn remote_history(&self, id: &str, field: Field) -> Vec<HistoryEntry> {
        let mut entries = self.journal.history(id, field).to_vec();
        let Some(remote) = self.r.get(id) else {
            return entries;
        };
        if let Some(intent) = remote_intent_of(remote, field)
            && !entries.iter().any(|entry| entry.intent == intent.id)
        {
            entries.push(HistoryEntry {
                intent: intent.id,
                applied_at_ms: intent.applied_at_ms,
                in_local: self.local_holds_remote(id, remote, field),
            });
        }
        entries
    }

    fn local_holds_remote(&self, id: &str, remote: &RemoteNode, field: Field) -> bool {
        (self.l.get(id)).is_some_and(|local| {
            same(
                remote_value(remote, field).as_ref(),
                self.local_value(id, &local.shape, field).as_ref(),
            )
        })
    }

    fn local_value(&self, id: &str, shape: &Shape, field: Field) -> Option<Value> {
        match field {
            Field::Place => {
                let mark = (self.unsent_marks.get(id).map(String::as_str)).or_else(|| {
                    (self.r.live(id))
                        .filter(|remote| remote.same_position(shape))
                        .and_then(|_| self.r.get(id)?.pending_for.as_deref())
                });
                Some(place(shape, mark))
            }
            Field::Content => content(shape, None),
            Field::Status => Some(Value::Status { live: true }),
        }
    }

    fn synced_value(&self, id: &str, field: Field) -> Option<Value> {
        let shape = self.s.get(id)?;
        match field {
            Field::Place => Some(place(shape, self.synced_mark(id))),
            Field::Content => content(shape, None),
            Field::Status => Some(Value::Status { live: true }),
        }
    }

    fn remote_intent(&self, id: &str, field: Field, value: &Value) -> Option<IntentId> {
        let remote = self.r.get(id)?;
        same(remote_value(remote, field).as_ref(), Some(value))
            .then(|| remote_intent_of(remote, field).map(|intent| intent.id))
            .flatten()
    }

    fn synced_intent(&self, id: &str, field: Field, value: &Value) -> Option<IntentId> {
        same(self.synced_value(id, field).as_ref(), Some(value))
            .then(|| self.synced_intents(id)?.get(field).map(|intent| intent.id))
            .flatten()
    }

    fn intent_of(&self, id: &str, field: Field, value: &Value) -> Option<IntentId> {
        (self.journal_row(id, field))
            .filter(|row| row.value.same(value))
            .map(|row| row.intent)
            .or_else(|| self.remote_intent(id, field, value))
            .or_else(|| self.synced_intent(id, field, value))
    }

    fn legacy_intents(&self, id: &str, node: &RemoteNode) -> NodeIntents {
        let kept_live = (self.r.get(id))
            .filter(|old| !old.trashed && !node.trashed)
            .and_then(|old| old.intents.as_ref()?.status.clone());
        NodeIntents {
            place: Some(FieldIntent::legacy(IntentId::legacy_place(
                id,
                node.meta_rev,
                &node.shape.parent,
                &nfc(&node.shape.name),
            ))),
            content: (node.shape.kind == Kind::File && !node.version_id.is_empty())
                .then(|| FieldIntent::legacy(IntentId::legacy_content(id, &node.version_id))),
            status: Some(kept_live.unwrap_or_else(|| {
                FieldIntent::legacy(match node.trashed {
                    true => IntentId::legacy_trashed(id, &node.trash_group),
                    false => IntentId::legacy_live(id, node.meta_rev),
                })
            })),
        }
    }

    pub(super) fn write_out_synced_intents(&mut self, id: &str) {
        if self.r.get(id).is_some()
            && self.s.get(id).is_some()
            && !self.synced_intents.contains_key(id)
        {
            let synced = self.synced_from_remote(id);
            self.put_synced_intents(id, Some(synced));
        }
    }

    pub(super) fn take_remote_intents(&mut self, id: &str, node: &mut RemoteNode) {
        let intents = (node.intents.take()).unwrap_or_else(|| self.legacy_intents(id, node));
        self.write_out_synced_intents(id);
        for field in Field::ALL {
            let Some(intent) = intents.get(field) else {
                continue;
            };
            let old = (self.r.get(id)).and_then(|old| {
                let held = remote_intent_of(old, field)?.clone();
                Some((held, self.local_holds_remote(id, old, field)))
            });
            if old.as_ref().is_some_and(|(old, _)| old.id == intent.id) {
                continue;
            }
            let in_local = self.local_holds_remote(id, node, field);
            if let Some((old, old_in_local)) = old {
                self.journal.remote_holds(id, field, &old, old_in_local);
            }
            if !self.journal.history(id, field).is_empty() || self.journal_row(id, field).is_some()
            {
                self.journal.remote_holds(id, field, intent, in_local);
            }
        }
        node.intents = Some(intents);
    }

    pub(super) fn note_local_holds(&mut self, id: &str) {
        let Some(remote) = self.r.get(id) else {
            return;
        };
        let held: Vec<(Field, IntentId)> = (Field::ALL.into_iter())
            .filter(|field| self.local_holds_remote(id, remote, *field))
            .filter_map(|field| Some((field, remote_intent_of(remote, field)?.id)))
            .collect();
        for (field, intent) in held {
            self.journal.local_holds(id, field, intent);
        }
    }

    pub(super) fn note_local_leaves(&mut self, id: &str, shape: &Shape) {
        let Some(remote) = self.r.get(id) else {
            return;
        };
        let left: Vec<(Field, FieldIntent)> = (Field::ALL.into_iter())
            .filter(|&field| {
                self.local_holds_remote(id, remote, field)
                    && !same(
                        remote_value(remote, field).as_ref(),
                        self.local_value(id, shape, field).as_ref(),
                    )
            })
            .filter_map(|field| Some((field, remote_intent_of(remote, field)?.clone())))
            .collect();
        for (field, intent) in left {
            self.journal.remote_holds(id, field, &intent, true);
        }
    }

    pub(super) fn record_user_change(&mut self, id: &str, node: &LocalNode) {
        let old = self.l.get(id).map(|old| old.shape.clone());
        let fields: &[Field] = match node.shape.kind {
            Kind::File => &[Field::Place, Field::Content],
            Kind::Dir => &[Field::Place],
        };
        for &field in fields {
            let Some(value) = self.local_value(id, &node.shape, field) else {
                continue;
            };
            let seen = match &old {
                Some(old) => self.local_value(id, old, field),
                None => (self.synced_value(id, field)).or_else(|| {
                    (self.r.live(id).and(self.r.get(id)))
                        .and_then(|remote| remote_value(remote, field))
                }),
            };
            if same(seen.as_ref(), Some(&value)) {
                continue;
            }
            let wire_base = match (field, &seen) {
                (Field::Place, Some(Value::Place(seen))) => WireBase::Place(seen.clone()),
                (Field::Content, _) => (self.content_provenance.get(id, &self.r))
                    .and_then(|provenance| {
                        let local = provenance.local.as_ref()?;
                        Some(WireBase::Content {
                            made_over: local.made_over.clone(),
                            source_version_id: Some(local.version_id.clone()),
                        })
                    })
                    .unwrap_or(WireBase::None),
                _ => WireBase::None,
            };
            let held = (self.r.get(id))
                .filter(|remote| same(remote_value(remote, field).as_ref(), seen.as_ref()))
                .and_then(|remote| remote_intent_of(remote, field).cloned());
            if let Some(held) = held {
                self.journal.remote_holds(id, field, &held, true);
            }
            let base = seen.and_then(|seen| self.intent_of(id, field, &seen));
            self.journal.user_change(id, value, wire_base, base);
        }
        if old.is_none() {
            self.write_out_journal(id);
            self.journal.user_revived(id);
            if self.r.live(id).is_none() && self.s.get(id).is_none() {
                let base = (self.r.get(id))
                    .and_then(|remote| remote_intent_of(remote, Field::Status))
                    .map(|intent| intent.id);
                (self.journal).user_change(id, Value::Status { live: true }, WireBase::None, base);
            }
        }
    }

    /// Takes a node its user deleted out of L. A node R or S holds gets a status row with the seen list of what L
    /// held for it and the nodes below it (RFC 0029 §5.1); the rows of a node neither holds end, since no server
    /// ever held it.
    pub fn remove_local_by_user(&mut self, id: &str) {
        if self.l.get(id).is_some() {
            if self.r.get(id).is_none() && self.s.get(id).is_none() {
                for field in Field::ALL {
                    self.journal.end(id, field);
                }
            } else {
                let seen_list = self.seen_list(id);
                let base = (self.r.live(id).and(self.r.get(id)))
                    .and_then(|remote| remote_intent_of(remote, Field::Status))
                    .map(|intent| intent.id)
                    .or_else(|| (self.synced_intents(id)?.status).map(|intent| intent.id));
                self.journal.user_delete(id, seen_list, base);
            }
        }
        self.remove_local(id);
    }

    fn seen_list(&self, id: &str) -> Vec<Seen> {
        let mut nodes: Vec<NodeId> = vec![id.to_owned()];
        let mut next = 0;
        while let Some(node) = nodes.get(next).cloned() {
            let deleted = (self.r_names.children(&node)).filter(|child| {
                self.l.get(child).is_none() && !self.journal.seen(child).is_empty()
            });
            let children: Vec<NodeId> = (self.l_names.children(&node))
                .chain(deleted)
                .cloned()
                .collect();
            nodes.extend(children);
            next += 1;
        }
        let mut seen = Vec::new();
        for node in &nodes {
            let Some(local) = self.l.get(node) else {
                seen.extend(self.journal.seen(node).iter().cloned());
                continue;
            };
            for field in Field::ALL {
                let value = self.local_value(node, &local.shape, field);
                let current = (value.as_ref()).and_then(|value| self.intent_of(node, field, value));
                seen.extend(
                    (self.remote_history(node, field).into_iter())
                        .filter(|entry| entry.in_local && Some(entry.intent) != current)
                        .map(|entry| Seen {
                            node: node.clone(),
                            field,
                            intent: entry.intent,
                            value: None,
                        }),
                );
                if let Some(intent) = current {
                    seen.push(Seen {
                        node: node.clone(),
                        field,
                        intent,
                        value,
                    });
                }
            }
        }
        seen
    }

    pub(super) fn note_synced_intents(&mut self, id: &str) {
        if self.s.get(id).is_none() {
            self.put_synced_intents(id, None);
            return;
        }
        let mut intents = self.synced_intents.get(id).cloned().unwrap_or_default();
        for field in Field::ALL {
            let Some(value) = self.synced_value(id, field) else {
                intents.set(field, None);
                continue;
            };
            let remote = (self.r.get(id))
                .filter(|remote| same(remote_value(remote, field).as_ref(), Some(&value)))
                .and_then(|remote| remote_intent_of(remote, field).cloned());
            let row = (self.journal_row(id, field))
                .filter(|row| row.value.same(&value))
                .map(|row| FieldIntent {
                    id: match row.state {
                        RowState::Held { held_as, .. } => held_as,
                        _ => row.intent,
                    },
                    author: String::new(),
                    replay: row.replay,
                    applied_at_ms: row.time_ms,
                });
            if let Some(intent) = remote.or(row) {
                intents.set(field, Some(intent));
            }
        }
        self.keep_synced_intents(id, intents);
    }

    pub fn stamp_intents(
        &mut self,
        ops: &mut [proto::Op],
        device_id: &str,
        head_time_ms: i64,
        wire: Wire,
    ) -> OpsWithoutIntent {
        let mut unrecorded = 0;
        for op in ops.iter_mut() {
            let Some(kind) = &op.op else {
                continue;
            };
            let (node, fields, carried) = op_fields(kind);
            let node = node.to_owned();
            let mut intents: Vec<(Field, proto::Intent)> = Vec::new();
            for (field, value) in fields {
                let local = |row: &Row| matches!(row.state, RowState::Local { .. });
                let row = match self.journal_row(&node, field) {
                    Some(row)
                        if local(&row) && carried && !same(Some(&row.value), value.as_ref()) =>
                    {
                        let own = (self.r.get(&node))
                            .and_then(|remote| remote_intent_of(remote, field))
                            .filter(|intent| intent.author == device_id);
                        if let Some(own) = own {
                            intents.push((
                                field,
                                proto::Intent {
                                    id: own.id.as_bytes().to_vec(),
                                    author_device_id: device_id.to_owned(),
                                    ..Default::default()
                                },
                            ));
                        }
                        None
                    }
                    Some(row) if local(&row) => {
                        self.journal.send(&node, field, head_time_ms).cloned()
                    }
                    Some(row) => Some(row.into_owned()),
                    None if carried => None,
                    None => value.map(|value| {
                        let base = (self.r.get(&node))
                            .and_then(|remote| remote_intent_of(remote, field))
                            .map(|intent| intent.id)
                            .or_else(|| {
                                self.synced_intents(&node)?
                                    .get(field)
                                    .map(|intent| intent.id)
                            });
                        (self.journal)
                            .send_unrecorded(&node, value, WireBase::None, base, head_time_ms)
                            .clone()
                    }),
                };
                if let Some(row) = row {
                    intents.push((field, wire_intent(&row, device_id)));
                }
            }
            if intents.is_empty() {
                unrecorded += 1;
                continue;
            }
            if wire == Wire::WithoutIntents {
                continue;
            }
            op.intent = Some(intents[0].1.clone());
            if matches!(kind, op::Op::CreateDir(_) | op::Op::CreateFile(_))
                && intents.iter().any(|(_, intent)| *intent != intents[0].1)
            {
                let mut fields = proto::FieldIntents::default();
                for (field, intent) in intents {
                    match field {
                        Field::Place => fields.place = Some(intent),
                        Field::Content => fields.content = Some(intent),
                        Field::Status => fields.status = Some(intent),
                    }
                }
                op.fields = Some(fields);
            }
            if let Some(op::Op::Delete(delete)) = &mut op.op {
                let view = delete_view(self.journal.seen(&node), &self.remote_fields_below(&node));
                delete.unseen = view
                    .unseen
                    .iter()
                    .map(|id| id.as_bytes().to_vec())
                    .collect();
                delete.held = view.held.iter().map(|id| id.as_bytes().to_vec()).collect();
            }
        }
        OpsWithoutIntent(unrecorded)
    }

    pub fn record_results(
        &mut self,
        commit: &proto::Commit,
        result: &proto::CommitResult,
    ) -> BTreeSet<NodeId> {
        use op_result::Code;
        let mut sent_again = BTreeSet::new();
        let states: BTreeMap<&str, &proto::NodeState> = (result.changes.iter())
            .filter_map(|change| match &change.change {
                Some(change_record::Change::Node(state)) => Some((state.node_id.as_str(), state)),
                _ => None,
            })
            .collect();
        let reported = |trees: &Trees,
                        id: &str,
                        field: Field|
         -> Option<(FieldIntent, Option<String>, bool)> {
            let node = match states.get(id) {
                Some(state) => {
                    let (_, mut node) = RemoteNode::from_state(state).ok()?;
                    let intents =
                        (node.intents.take()).unwrap_or_else(|| trees.legacy_intents(id, &node));
                    node.intents = Some(intents);
                    node
                }
                None => trees.r.get(id)?.clone(),
            };
            let mut intent = remote_intent_of(&node, field)?.clone();
            if intent.applied_at_ms == 0 {
                intent.applied_at_ms = result.committed_at_ms;
            }
            let version_id = (field == Field::Content && !node.version_id.is_empty())
                .then(|| node.version_id.clone());
            Some((intent, version_id, !node.trashed))
        };
        for (op, outcome) in commit.ops.iter().zip(&result.results) {
            let Some(kind) = &op.op else {
                continue;
            };
            let (node, fields, _) = op_fields(kind);
            let node = node.to_owned();
            let code = Code::try_from(outcome.code).unwrap_or(Code::Unspecified);
            if code == Code::MergedInto {
                self.journal.merged_into(&node, &outcome.node_id);
                continue;
            }
            for (field, value) in fields {
                let Some(sent) = self.journal.sent_intent(&node, field) else {
                    continue;
                };
                match code {
                    Code::Applied | Code::AppliedRenamed => {
                        if let Some((held, version_id, _)) = reported(self, &node, field) {
                            self.journal.applied(&node, field, sent, &held, version_id);
                        }
                    }
                    Code::ConflictCopy if field == Field::Content => {
                        let version_id = reported(self, &outcome.node_id, field)
                            .and_then(|(_, version, _)| version);
                        let at_ms = result.committed_at_ms;
                        (self.journal).held_elsewhere(
                            &node,
                            field,
                            sent,
                            &outcome.node_id,
                            version_id,
                            at_ms,
                        );
                    }
                    Code::Superseded => {
                        let elsewhere = (std::iter::once(outcome.node_id.as_str()))
                            .filter(|id| !id.is_empty())
                            .chain(states.keys().copied())
                            .filter(|id| *id != node)
                            .find_map(|id| {
                                (reported(self, id, field))
                                    .filter(|(held, _, _)| held.id == sent)
                                    .map(|reported| (id.to_owned(), reported))
                            });
                        if let Some((target, (held, version_id, _))) = elsewhere {
                            (self.journal).held_elsewhere(
                                &node,
                                field,
                                sent,
                                &target,
                                version_id,
                                held.applied_at_ms,
                            );
                            continue;
                        }
                        let own = reported(self, &node, field);
                        let first = (self.journal_row(&node, field))
                            .filter(|row| row.intent == sent)
                            .zip(own.as_ref())
                            .is_some_and(|(row, (held, _, live))| {
                                *live && held.applied_at_ms < row.time_ms
                            });
                        match first {
                            true => {
                                self.journal.unsend(&node, field);
                                sent_again.insert(node.clone());
                            }
                            false => self.journal.superseded(
                                &node,
                                field,
                                sent,
                                own.map(|(held, _, _)| held.id),
                            ),
                        }
                    }
                    Code::Rejected
                        if outcome.reject_reason == "ID_EXISTS"
                            && value == Some(Value::Status { live: true }) =>
                    {
                        self.journal.end(&node, field);
                    }
                    Code::Rejected => self.journal.unsend(&node, field),
                    _ => {}
                }
            }
        }
        let touched: BTreeSet<NodeId> = (commit.ops.iter().zip(&result.results))
            .filter_map(|(op, outcome)| {
                Some([
                    op_fields(op.op.as_ref()?).0.to_owned(),
                    outcome.node_id.clone(),
                ])
            })
            .flatten()
            .filter(|node| !node.is_empty())
            .collect();
        for node in touched {
            self.compact_journal(&node, Compact::AfterChange);
        }
        sent_again
    }

    fn remote_fields_below(&self, id: &str) -> Vec<RemoteField> {
        let mut nodes: Vec<NodeId> = vec![id.to_owned()];
        let mut next = 0;
        while let Some(node) = nodes.get(next).cloned() {
            nodes.extend(self.r_names.children(&node).cloned());
            next += 1;
        }
        let mut fields = Vec::new();
        for node in nodes {
            let Some(remote) = self.r.get(&node) else {
                continue;
            };
            for field in Field::ALL {
                if let (Some(intent), Some(value)) =
                    (remote_intent_of(remote, field), remote_value(remote, field))
                {
                    fields.push(RemoteField {
                        node: node.clone(),
                        field,
                        intent: intent.id,
                        value,
                    });
                }
            }
        }
        fields
    }
}
