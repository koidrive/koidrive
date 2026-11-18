// SPDX-License-Identifier: Apache-2.0

//! The journal's hooks in the three trees (RFC 0029 §5.1): the intents R and S hold, R's history, and the rows the
//! user's changes write. R's history and S's intents follow R's intents implicitly; the journal keeps a history,
//! and Trees keeps S's intents, only once they differ from what R's intents give.

use koidrive_proto::v1::{self as proto, op};

use crate::journal::{
    Author, Field, FieldIntent, HistoryEntry, IntentId, Journal, NodeIntents, Place, RemoteField,
    Row, RowState, Seen, Value, WireBase, delete_view,
};
use crate::names::nfc;
use crate::state::ContentValue;
use crate::tree::{Kind, LocalNode, NodeId, RemoteNode, Shape};

use super::Trees;

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
            Author::Carried(author) => author.clone(),
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

    /// R's history of the field: the entries the journal keeps, then R's intent when the journal does not hold
    /// it, marked when L holds its value.
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
        (self.journal.row(id, field))
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
            if !self.journal.history(id, field).is_empty() || self.journal.row(id, field).is_some()
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
            let row = (self.journal.row(id, field))
                .filter(|row| row.value.same(&value))
                .map(|row| FieldIntent {
                    id: match row.state {
                        RowState::Held { held_as, .. } => held_as,
                        _ => row.intent,
                    },
                    author: String::new(),
                    carried: row.replay,
                    applied_at_ms: row.time_ms,
                });
            if let Some(intent) = remote.or(row) {
                intents.set(field, Some(intent));
            }
        }
        self.keep_synced_intents(id, intents);
    }

    /// Puts the rows of the fields each op sets in the commit being built (RFC 0029 §5.2): a `local` row becomes
    /// `sent` at `time_ms`, R's head time, and an op no row covers gets an own `sent` row over the intent R or S
    /// holds for the field, unless an earlier rule made it carry another device's value. With `wire`, the session's
    /// server records intents and each op names them, with the view of each `Delete` (§5.3). Returns how many ops
    /// went without an intent.
    pub fn stamp_intents(
        &mut self,
        ops: &mut [proto::Op],
        device_id: &str,
        time_ms: i64,
        wire: bool,
    ) -> usize {
        let mut unrecorded = 0;
        for op in ops.iter_mut() {
            let Some(kind) = &op.op else {
                continue;
            };
            let (node, fields, carried) = op_fields(kind);
            let node = node.to_owned();
            let mut rows: Vec<(Field, Row)> = Vec::new();
            for (field, value) in fields {
                let row = match self.journal.row(&node, field) {
                    Some(row) if matches!(row.state, RowState::Local { .. }) => {
                        self.journal.send(&node, field, time_ms).cloned()
                    }
                    Some(row) => Some(row.clone()),
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
                            .send_unrecorded(&node, value, WireBase::None, base, time_ms)
                            .clone()
                    }),
                };
                if let Some(row) = row {
                    rows.push((field, row));
                }
            }
            if rows.is_empty() {
                unrecorded += 1;
                continue;
            }
            if !wire {
                continue;
            }
            let intents: Vec<(Field, proto::Intent)> = (rows.iter())
                .map(|(field, row)| (*field, wire_intent(row, device_id)))
                .collect();
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
        unrecorded
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
