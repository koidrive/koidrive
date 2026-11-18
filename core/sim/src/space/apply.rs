// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet, HashMap};

use koidrive_engine::names::{conflict_name, is_reserved_at_root, name_key, normalize_name};
use koidrive_proto::v1::{
    ChangeRecord, ChunkRef, Content, CreateDir, CreateFile, Delete, Error, Move, NodeKind,
    NodePurged, NodeStatus, Op, OpResult, Place, Restore, change_record, error, op, op_result,
};

use super::intents::{
    FIELDS, Field, FieldIntent, Incoming, IntentId, NodeIntents, OpIntents, Sender, Tombstone,
    resolve,
};
use super::{ChunkState, Node, Session, State, StoredResult, Version, fail};
use crate::world::same_bytes;

pub const MAX_DEPTH: usize = 256;

pub(super) struct Context<'a> {
    pub seq: u64,
    pub commit_id: &'a str,
    pub base_seq: u64,
    pub now_ms: i64,
    pub committer: &'a Session,
    pub copies_created_files: bool,
    pub results: &'a HashMap<String, StoredResult>,
}

type Reject = &'static str;

fn result(code: op_result::Code, node_id: &str, name: &str) -> OpResult {
    OpResult {
        code: code as i32,
        node_id: node_id.into(),
        name: name.into(),
        reject_reason: String::new(),
    }
}

fn rejected(reason: Reject) -> OpResult {
    OpResult {
        code: op_result::Code::Rejected as i32,
        reject_reason: reason.into(),
        ..Default::default()
    }
}

fn superseded() -> OpResult {
    result(op_result::Code::Superseded, "", "")
}

fn unchanged(node: &Node) -> OpResult {
    result(op_result::Code::Applied, "", &node.name)
}

fn bound_made_over(ids: &[String]) -> Vec<String> {
    let mut bound: Vec<String> = Vec::new();
    for id in ids {
        if bound.len() < 64 && !id.is_empty() && !bound.contains(id) {
            bound.push(id.clone());
        }
    }
    bound
}

struct Chain {
    made_over: Vec<String>,
    source: String,
}

fn chain(source: &str, bases: &[String]) -> Chain {
    let ids: Vec<String> = std::iter::once(source.to_owned())
        .chain(bases.iter().cloned())
        .collect();
    Chain {
        made_over: bound_made_over(&ids),
        source: source.to_owned(),
    }
}

struct DeleteView {
    base_seq: u64,
    unseen: BTreeSet<IntentId>,
    held: Vec<IntentId>,
    seen_content: Vec<IntentId>,
}

struct LostContent {
    node: Node,
    content: FieldIntent,
    place_author: Option<String>,
}

fn applied(renamed: bool, name: &str) -> OpResult {
    let code = if renamed {
        op_result::Code::AppliedRenamed
    } else {
        op_result::Code::Applied
    };
    result(code, "", name)
}

enum Deleted {
    Trash(Box<Node>),
    Purged(Box<Tombstone>),
}

pub(super) struct Applier<'a> {
    st: &'a mut State,
    ctx: Context<'a>,
    remap: HashMap<String, String>,
    touched: Vec<String>,
    touched_set: BTreeSet<String>,
    targets: BTreeSet<String>,
    named: BTreeSet<String>,
    revived_ancestors: BTreeMap<String, Node>,
    purged: Vec<ChangeRecord>,
    intents: OpIntents,
}

impl<'a> Applier<'a> {
    pub fn new(st: &'a mut State, ctx: Context<'a>) -> Self {
        Self {
            st,
            ctx,
            remap: HashMap::new(),
            touched: Vec::new(),
            touched_set: BTreeSet::new(),
            targets: BTreeSet::new(),
            named: BTreeSet::new(),
            revived_ancestors: BTreeMap::new(),
            purged: Vec::new(),
            intents: OpIntents::default(),
        }
    }

    pub fn apply(mut self, ops: &[Op]) -> Result<(Vec<OpResult>, Vec<ChangeRecord>), Error> {
        let mut results = Vec::with_capacity(ops.len());
        for (index, op) in ops.iter().enumerate() {
            let sender = Sender {
                device_id: &self.ctx.committer.device_id,
                seq: self.ctx.seq,
                now_ms: self.ctx.now_ms,
            };
            self.intents = resolve(op, &sender, &mut self.st.intent_ids);
            results.push(self.apply_op(op, index)?);
            let named = match &op.op {
                Some(op::Op::CreateDir(op)) => Some(&op.node_id),
                Some(op::Op::CreateFile(op)) => Some(&op.node_id),
                Some(op::Op::UpdateContent(op)) => Some(&op.node_id),
                Some(op::Op::Move(op)) => Some(&op.node_id),
                Some(op::Op::Restore(op)) => Some(&op.node_id),
                Some(op::Op::Delete(_)) | Some(op::Op::MarkGitTracked(_)) | None => None,
            };
            if let Some(node_id) = named {
                self.named.insert(self.id(node_id));
            }
        }
        self.retrash_unneeded_ancestors();
        let changes = (self.touched.iter())
            .map(|id| ChangeRecord {
                change: Some(change_record::Change::Node(
                    self.st.node_state(&self.st.nodes[id]),
                )),
            })
            .chain(self.purged)
            .collect();
        Ok((results, changes))
    }

    fn apply_op(&mut self, op: &Op, index: usize) -> Result<OpResult, Error> {
        match &op.op {
            Some(op::Op::CreateDir(op)) => self.create_dir(op),
            Some(op::Op::CreateFile(op)) => self.create_file(op),
            Some(op::Op::UpdateContent(op)) => {
                let content = op.content.as_ref().expect("checked before applying");
                self.update_content(
                    &op.node_id,
                    content,
                    &content.chunks,
                    op.resend,
                    &op.made_over,
                    &op.source_version_id,
                )
            }
            Some(op::Op::Move(op)) => self.move_node(op),
            Some(op::Op::Delete(op)) => Ok(self.delete(op, index)),
            Some(op::Op::Restore(op)) => self.restore(op),
            Some(op::Op::MarkGitTracked(_)) | None => Err(fail(
                error::Code::InvalidArgument,
                format!("op {index} is empty"),
            )),
        }
    }

    fn id(&self, node_id: &str) -> String {
        self.remap
            .get(node_id)
            .cloned()
            .unwrap_or_else(|| node_id.to_string())
    }

    fn node(&self, node_id: &str) -> Option<Node> {
        self.st.nodes.get(&self.id(node_id)).cloned()
    }

    fn dev(&self) -> &str {
        &self.ctx.committer.device_id
    }

    fn touch(&mut self, node_id: &str) {
        self.report(node_id);
        self.targets.insert(node_id.to_string());
    }

    fn report(&mut self, node_id: &str) {
        if self.touched_set.insert(node_id.to_string()) {
            self.touched.push(node_id.to_string());
        }
    }

    fn unseen_content(&self, node: &Node) -> bool {
        node.content_rev > self.ctx.base_seq && node.content_dev != self.dev()
    }

    fn unseen_edit(&self, node: &Node) -> bool {
        node.edit_rev > self.ctx.base_seq && node.edit_dev != self.dev()
    }

    fn unseen_meta(&self, node: &Node) -> bool {
        node.meta_edit_rev > self.ctx.base_seq && node.meta_edit_dev != self.dev()
    }

    /// RFC 0002 section 9.
    fn unseen_parent(&self, node: &Node) -> bool {
        node.parent_rev > self.ctx.base_seq && node.parent_dev != self.dev()
    }

    fn client(&self) -> bool {
        self.intents.client
    }

    fn incoming(&self, field: Field) -> &Incoming {
        self.intents.field(field)
    }

    fn set_intent(&mut self, node_id: &str, field: Field, intent: FieldIntent) {
        let seq = self.ctx.seq;
        self.st
            .nodes
            .get_mut(node_id)
            .expect("a node with intents exists")
            .intents
            .set(field, FieldIntent { seq, ..intent });
    }

    fn set_incoming(&mut self, node_id: &str, field: Field) {
        let intent = self.incoming(field).intent.clone();
        self.set_intent(node_id, field, intent);
    }

    fn minted(&mut self, author: &str, carried: bool) -> FieldIntent {
        FieldIntent {
            id: IntentId::mint(&mut self.st.intent_ids, self.ctx.now_ms),
            author: author.to_owned(),
            carried,
            applied_at_ms: self.ctx.now_ms,
            seq: self.ctx.seq,
        }
    }

    fn holds_intents(&self, node_id: &str, fields: &[Field]) -> bool {
        let held = &self.st.nodes[node_id].intents;
        fields
            .iter()
            .all(|&field| held.id(field) == Some(self.incoming(field).intent.id))
    }

    fn held_elsewhere(&mut self, node_id: &str, fields: &[Field]) -> bool {
        if !self.client() {
            return false;
        }
        for &field in fields {
            let incoming = self.incoming(field);
            if !incoming.replay {
                continue;
            }
            let holder = (self.st.holders(incoming.intent.id).into_iter())
                .find(|(holder, _)| holder != node_id);
            if let Some((holder, _)) = holder {
                self.touch(&holder);
                return true;
            }
        }
        false
    }

    fn over_held(&self, node_id: &str, field: Field) -> bool {
        self.intents
            .over
            .is_none_or(|over| self.st.nodes[node_id].intents.id(field) == Some(over))
    }

    fn trash_knows(&self, node_id: &str, id: IntentId) -> bool {
        let node = &self.st.nodes[node_id];
        node.intents.holds(id)
            || node.status_before == Some(id)
            || self
                .st
                .trash_held_of(node.trash_group.as_deref())
                .contains(&id)
    }

    fn purged_replay(&self, node_id: &str, create: bool) -> Option<Tombstone> {
        let sent = FIELDS.iter().any(|&field| {
            let incoming = self.incoming(field);
            incoming.replay || incoming.intent.carried
        });
        if !create && !sent {
            return None;
        }
        self.st.tombstones.get(node_id).cloned()
    }

    fn root(&self) -> Node {
        self.st
            .nodes
            .values()
            .find(|n| n.parent_id.is_none())
            .expect("the space has a root")
            .clone()
    }

    fn nearest_live(&self, node_id: Option<&str>) -> Node {
        let mut cur = node_id.map(str::to_owned);
        for _ in 0..=MAX_DEPTH {
            let Some(id) = cur else {
                break;
            };
            cur = match self.st.nodes.get(&id) {
                Some(node) if node.status == NodeStatus::Live => return node.clone(),
                Some(node) => node.parent_id.clone(),
                None => (self.st.tombstones.get(&id)).and_then(|t| t.parent_id.clone()),
            };
        }
        self.root()
    }

    fn holds(&self, node: &Node, content: &Content) -> bool {
        (node.cur_version_id.as_ref())
            .is_some_and(|id| same_bytes(&self.st.versions[id].content, content))
    }

    fn depth(&self, node: &Node) -> usize {
        let mut depth = 0;
        let mut cur = node.parent_id.as_ref().and_then(|id| self.st.nodes.get(id));
        while let Some(n) = cur {
            depth += 1;
            if depth > MAX_DEPTH {
                break;
            }
            cur = n.parent_id.as_ref().and_then(|id| self.st.nodes.get(id));
        }
        depth
    }

    fn check_depth(&self, parent: &Node) -> Result<(), Error> {
        if self.depth(parent) + 1 > MAX_DEPTH {
            return Err(fail(
                error::Code::LimitExceeded,
                format!("path depth would exceed {MAX_DEPTH}"),
            ));
        }
        Ok(())
    }

    fn check_subtree_depth(&self, node: &Node, parent: &Node) -> Result<(), Error> {
        let new_depth = self.depth(parent) + 1;
        if new_depth <= self.depth(node) {
            return Ok(());
        }
        let mut level = BTreeSet::from([node.node_id.clone()]);
        let mut height = 0;
        while new_depth + height <= MAX_DEPTH {
            level = self
                .st
                .nodes
                .values()
                .filter(|n| n.parent_id.as_ref().is_some_and(|p| level.contains(p)))
                .map(|n| n.node_id.clone())
                .collect();
            if level.is_empty() {
                return Ok(());
            }
            height += 1;
        }
        Err(fail(
            error::Code::LimitExceeded,
            format!("path depth would exceed {MAX_DEPTH}"),
        ))
    }

    fn is_root(&self, node_id: &str) -> bool {
        self.st
            .nodes
            .get(node_id)
            .is_some_and(|n| n.parent_id.is_none())
    }

    fn live_with_name(&self, parent_id: &str, key: &str, exclude_id: &str) -> Option<&Node> {
        self.st
            .nodes
            .values()
            .filter(|n| {
                n.parent_id.as_deref() == Some(parent_id)
                    && n.name_key == key
                    && n.status == NodeStatus::Live
                    && n.node_id != exclude_id
            })
            .min_by_key(|n| n.row)
    }

    fn pick_name(&self, parent_id: &str, desired: &str, exclude_id: &str) -> (String, bool) {
        let at_root = self.is_root(parent_id);
        let mut name = desired.to_string();
        let mut attempt = 1;
        while self
            .live_with_name(parent_id, &name_key(&name), exclude_id)
            .is_some()
            || (at_root && is_reserved_at_root(&name))
        {
            name = conflict_name(
                desired,
                &self.ctx.committer.device_name,
                self.ctx.now_ms,
                attempt,
            );
            attempt += 1;
        }
        let renamed = name != desired;
        (name, renamed)
    }

    fn pick_conflict_copy_name(&self, parent_id: &str, original: &str) -> String {
        (1..)
            .map(|attempt| {
                conflict_name(
                    original,
                    &self.ctx.committer.device_name,
                    self.ctx.now_ms,
                    attempt,
                )
            })
            .find(|name| {
                self.live_with_name(parent_id, &name_key(name), "")
                    .is_none()
            })
            .expect("an unbounded search finds a free name")
    }

    fn revive_one(
        &mut self,
        node_id: &str,
        parent_id: &str,
        desired: &str,
        parent_dev: &str,
    ) -> (String, bool) {
        let (name, renamed) = self.pick_name(parent_id, desired, node_id);
        let (seq, dev) = (self.ctx.seq, self.dev().to_string());
        let node = self.st.nodes.get_mut(node_id).expect("revived node exists");
        let placed = node.parent_id.as_deref() != Some(parent_id) || node.name != name;
        node.status = NodeStatus::Live;
        node.status_before = None;
        node.trashed_at_ms = None;
        node.parent_id = Some(parent_id.to_string());
        node.parent_rev = seq;
        node.parent_dev = parent_dev.to_string();
        node.name_key = name_key(&name);
        node.name = name.clone();
        node.trash_group = None;
        node.meta_rev = seq;
        node.meta_edit_rev = seq;
        node.meta_edit_dev = dev.clone();
        node.meta_dev = dev;
        self.set_incoming(node_id, Field::Status);
        if placed {
            self.set_incoming(node_id, Field::Place);
        }
        self.touch(node_id);
        (name, renamed)
    }

    fn revive_chain(&mut self, node_id: &str) {
        let mut chain = Vec::new();
        let mut cur = self.st.nodes.get(node_id);
        while let Some(n) = cur.filter(|n| n.status == NodeStatus::Trashed) {
            chain.push(n.clone());
            cur = self.st.nodes.get(n.parent_id.as_deref().unwrap_or(""));
        }
        for n in chain.into_iter().rev() {
            let ancestor = !self.targets.contains(&n.node_id);
            if ancestor {
                self.revived_ancestors.insert(n.node_id.clone(), n.clone());
            }
            self.revive_one(
                &n.node_id,
                n.parent_id.as_deref().expect("a trashed node has a parent"),
                &n.name,
                "",
            );
            if ancestor {
                self.targets.remove(&n.node_id);
            }
        }
    }

    fn retrash_unneeded_ancestors(&mut self) {
        let mut needed = BTreeSet::new();
        for target in self.targets.iter().chain(&self.named) {
            let mut cur = self.st.nodes.get(target);
            while let Some(node) = cur.filter(|n| n.status == NodeStatus::Live) {
                if !needed.insert(node.node_id.clone()) {
                    break;
                }
                cur = node.parent_id.as_ref().and_then(|id| self.st.nodes.get(id));
            }
        }
        for (node_id, before) in std::mem::take(&mut self.revived_ancestors) {
            if self.st.nodes[&node_id].status == NodeStatus::Live && !needed.contains(&node_id) {
                self.st.nodes.insert(node_id, before);
            }
        }
    }

    fn revive_group(&mut self, parent_id: &str, group: &str) {
        let children = self.st.children(parent_id, NodeStatus::Trashed);
        for child in children
            .into_iter()
            .filter(|c| c.trash_group.as_deref() == Some(group))
        {
            self.revive_one(&child.node_id, parent_id, &child.name, "");
            self.revive_group(&child.node_id, group);
        }
    }

    fn revive_target(&mut self, node: Node) -> (Node, bool) {
        if node.status != NodeStatus::Trashed {
            return (node, false);
        }
        let group = node
            .trash_group
            .clone()
            .expect("a trashed node has a trash group");
        let parent_id = node.parent_id.clone().expect("a trashed node has a parent");
        self.revive_chain(&parent_id);
        let (_, renamed) = self.revive_one(&node.node_id, &parent_id, &node.name, "");
        self.revive_group(&node.node_id, &group);
        (self.st.nodes[&node.node_id].clone(), renamed)
    }

    fn place_in(&self, parent_id: &str) -> Option<Node> {
        if let Some(parent) = self.node(parent_id) {
            return Some(parent);
        }
        let tombstone = self.st.tombstones.get(&self.id(parent_id))?;
        let live = self.nearest_live(tombstone.parent_id.as_deref());
        if self.depth(&live) < MAX_DEPTH {
            return Some(live);
        }
        let parent = live.parent_id.as_ref().expect("a deep folder has a parent");
        Some(self.st.nodes[parent].clone())
    }

    fn parent_row(&self, parent_id: &str) -> Result<Result<Node, Reject>, Error> {
        let Some(parent) = self.place_in(parent_id) else {
            return Ok(Err("NOT_FOUND"));
        };
        if parent.kind != NodeKind::Dir {
            return Ok(Err("PARENT_NOT_DIR"));
        }
        self.check_depth(&parent)?;
        Ok(Ok(parent))
    }

    fn parent_for(&mut self, parent_id: &str) -> Result<Result<Node, Reject>, Error> {
        let parent = self.parent_row(parent_id)?;
        if let Ok(found) = &parent
            && found.status == NodeStatus::Trashed
        {
            self.revive_chain(&found.node_id);
        }
        Ok(parent)
    }

    fn insert_node(
        &mut self,
        node_id: &str,
        parent_id: &str,
        name: &str,
        kind: NodeKind,
        version_id: Option<String>,
        parent_dev: &str,
    ) {
        let row = self.st.next_row();
        let node = Node {
            node_id: node_id.into(),
            parent_id: Some(parent_id.into()),
            name: name.into(),
            name_key: name_key(name),
            kind,
            status: NodeStatus::Live,
            content_rev: self.ctx.seq,
            content_dev: self.dev().into(),
            edit_rev: self.ctx.seq,
            edit_dev: self.dev().into(),
            meta_rev: self.ctx.seq,
            meta_dev: self.dev().into(),
            meta_edit_rev: self.ctx.seq,
            meta_edit_dev: self.dev().into(),
            parent_rev: self.ctx.seq,
            parent_dev: parent_dev.into(),
            cur_version_id: version_id,
            trash_group: None,
            pending_for: None,
            pending_by: None,
            intents: Default::default(),
            status_before: None,
            copy_mark: None,
            trashed_at_ms: None,
            row,
        };
        self.st.nodes.insert(node_id.into(), node);
        self.st.tombstones.remove(node_id);
        self.touch(node_id);
    }

    fn made_over(&self, node_id: &str, ids: &[String], source: &str) -> Vec<String> {
        let mut ancestry = Vec::new();
        let mut seen = BTreeSet::new();
        let mut pending: Vec<_> = ids.iter().rev().cloned().collect();
        while ancestry.len() < 64
            && let Some(id) = pending.pop()
        {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(version) = self.st.versions.get(&id).filter(|v| v.node_id == node_id) {
                pending.extend(version.made_over.iter().rev().cloned());
            }
            ancestry.push(id);
        }
        if !source.is_empty() {
            ancestry.retain(|id| id != source);
            ancestry.insert(0, source.to_owned());
            ancestry.truncate(64);
        }
        ancestry
    }

    fn set_lineage(&mut self, version_id: &str, lineage: Chain) {
        let version = self.st.versions.get_mut(version_id).unwrap();
        version.made_over = lineage.made_over;
        version.source_version_id = lineage.source;
    }

    fn insert_version(&mut self, node_id: &str, content: &Content, chunks: &[ChunkRef]) -> String {
        let version_id = self.st.ids.uuid_v7(self.ctx.now_ms as u64);
        let content = Content {
            chunks: chunks.to_vec(),
            chunk_count: chunks.len() as u32,
            ..content.clone()
        };
        self.st.versions.insert(
            version_id.clone(),
            Version {
                version_id: version_id.clone(),
                node_id: node_id.into(),
                seq: self.ctx.seq,
                content,
                device_id: self.dev().into(),
                user_id: self.ctx.committer.user_id.clone(),
                created_at_ms: self.ctx.now_ms,
                made_over: Vec::new(),
                source_version_id: String::new(),
            },
        );
        let distinct: BTreeSet<&[u8]> = chunks.iter().map(|c| c.hash.as_slice()).collect();
        for hash in distinct {
            if let Some(row) = self.st.chunks.get_mut(hash) {
                row.state = ChunkState::Live;
            }
        }
        version_id
    }

    fn mark(&mut self, node_id: &str, pending_for: Option<String>) {
        let dev = self.dev().to_string();
        let node = self
            .st
            .nodes
            .get_mut(node_id)
            .expect("a marked node exists");
        node.pending_for = pending_for.filter(|stem| name_key(stem) != node.name_key);
        node.pending_by = node.pending_for.as_ref().map(|_| dev);
    }

    fn conflict_copy(
        &mut self,
        parent: &Node,
        original: &str,
        content: &Content,
        chunks: &[ChunkRef],
        lineage: Chain,
        intent: FieldIntent,
    ) -> OpResult {
        let home = if self.depth(parent) < MAX_DEPTH {
            parent.clone()
        } else {
            self.st.nodes[parent
                .parent_id
                .as_ref()
                .expect("a deep folder has a parent")]
            .clone()
        };
        let copy_id = self.st.ids.uuid_v7(self.ctx.now_ms as u64);
        let name = self.pick_conflict_copy_name(&home.node_id, original);
        let version_id = self.insert_version(&copy_id, content, chunks);
        self.set_lineage(&version_id, lineage);
        self.insert_node(
            &copy_id,
            &home.node_id,
            &name,
            NodeKind::File,
            Some(version_id),
            "",
        );
        let author = if intent.carried {
            intent.author.clone()
        } else {
            self.dev().to_string()
        };
        let place = self.minted(&author, intent.carried);
        let status = self.minted(&author, intent.carried);
        if intent.carried {
            self.st.nodes.get_mut(&copy_id).unwrap().copy_mark = Some((place.id, status.id));
        }
        self.set_intent(&copy_id, Field::Content, intent);
        self.set_intent(&copy_id, Field::Place, place);
        self.set_intent(&copy_id, Field::Status, status);
        result(op_result::Code::ConflictCopy, &copy_id, &name)
    }

    fn copy_of(&mut self, node: &Node, parent: &Node, intent: FieldIntent) {
        let version = self.st.versions[node
            .cur_version_id
            .as_ref()
            .expect("a lost file has content")]
        .clone();
        self.conflict_copy(
            parent,
            &node.name,
            &version.content,
            &version.content.chunks,
            Chain {
                made_over: version.made_over,
                source: version.source_version_id,
            },
            intent,
        );
    }

    fn same_create(
        &self,
        existing: &Node,
        parent_id: &str,
        name: &str,
        content: Option<&Content>,
    ) -> bool {
        let fields: &[Field] = match content {
            Some(_) => &FIELDS,
            None => &[Field::Place, Field::Status],
        };
        self.client()
            && existing.status == NodeStatus::Live
            && existing.parent_id.as_deref() == Some(self.id(parent_id).as_str())
            && existing.name == name
            && content.is_none_or(|content| self.holds(existing, content))
            && self.holds_intents(&existing.node_id, fields)
    }

    fn deleted_parent(&self, parent_id: &str) -> Option<Deleted> {
        let (place, status) = (self.incoming(Field::Place), self.incoming(Field::Status));
        if !self.client() || !place.intent.carried || !status.intent.carried {
            return None;
        }
        if let Some(parent) = self.node(parent_id) {
            return (parent.status == NodeStatus::Trashed && parent.kind == NodeKind::Dir)
                .then(|| Deleted::Trash(Box::new(parent)));
        }
        (self.st.tombstones.get(&self.id(parent_id)))
            .filter(|tombstone| {
                tombstone.knows(place.intent.id) || tombstone.knows(status.intent.id)
            })
            .map(|tombstone| Deleted::Purged(Box::new(tombstone.clone())))
    }

    fn deleted_knows(&self, deleted: &Deleted, id: IntentId) -> bool {
        match deleted {
            Deleted::Trash(trash) => self.trash_knows(&trash.node_id, id),
            Deleted::Purged(tombstone) => tombstone.knows(id),
        }
    }

    fn create_into_deleted(
        &mut self,
        node_id: &str,
        parent_id: &str,
        name: &str,
        content: Option<(&Content, Chain)>,
        pending_for: Option<String>,
    ) -> Option<OpResult> {
        let deleted = self.deleted_parent(parent_id)?;
        let intent = self.incoming(Field::Content).intent.clone();
        let seen = content.is_none() || self.deleted_knows(&deleted, intent.id);
        if let Deleted::Trash(trash) = &deleted
            && seen
            && self.depth(trash) < MAX_DEPTH
        {
            let dev = self.dev().to_string();
            let (kind, version_id, fields): (_, _, &[Field]) = match content {
                Some((content, lineage)) => {
                    let version_id = self.insert_version(node_id, content, &content.chunks);
                    self.set_lineage(&version_id, lineage);
                    (NodeKind::File, Some(version_id), &FIELDS)
                }
                None => (NodeKind::Dir, None, &[Field::Place, Field::Status]),
            };
            self.insert_node(node_id, &trash.node_id, name, kind, version_id, &dev);
            let node = self
                .st
                .nodes
                .get_mut(node_id)
                .expect("the node was inserted");
            node.status = NodeStatus::Trashed;
            node.trash_group = trash.trash_group.clone();
            node.trashed_at_ms = Some(self.ctx.now_ms);
            for &field in fields {
                self.set_incoming(node_id, field);
            }
            self.mark(node_id, pending_for);
            return Some(superseded());
        }
        if seen || !matches!(deleted, Deleted::Trash(_)) {
            self.leave_tombstone(node_id, parent_id, name, &deleted, content.is_some());
        }
        let Some((content, lineage)) = content.filter(|_| !seen) else {
            return Some(superseded());
        };
        let home = self.nearest_live(Some(&self.id(parent_id)));
        Some(self.conflict_copy(&home, name, content, &content.chunks, lineage, intent))
    }

    fn leave_tombstone(
        &mut self,
        node_id: &str,
        parent_id: &str,
        name: &str,
        deleted: &Deleted,
        file: bool,
    ) {
        let held: BTreeSet<IntentId> = match deleted {
            Deleted::Trash(trash) => (self.st.trash_held_of(trash.trash_group.as_deref()))
                .into_iter()
                .collect(),
            Deleted::Purged(tombstone) => tombstone.held.clone(),
        };
        let fields: &[Field] = if file {
            &FIELDS
        } else {
            &[Field::Place, Field::Status]
        };
        let mut intents = NodeIntents::default();
        for &field in fields {
            let intent = self.incoming(field).intent.clone();
            intents.set(
                field,
                FieldIntent {
                    seq: self.ctx.seq,
                    ..intent
                },
            );
        }
        let parent_id = self.id(parent_id);
        self.purged.push(ChangeRecord {
            change: Some(change_record::Change::Purged(NodePurged {
                node_id: node_id.to_owned(),
                last_parent_id: parent_id.clone(),
                intents: intents.wire(None),
                held: held.iter().map(|id| id.wire()).collect(),
            })),
        });
        self.st.tombstones.insert(
            node_id.to_owned(),
            Tombstone {
                parent_id: Some(parent_id),
                name: name.to_owned(),
                intents,
                held,
            },
        );
    }

    fn create_dir(&mut self, op: &CreateDir) -> Result<OpResult, Error> {
        let Some(name) = normalize_name(&op.name) else {
            return Ok(rejected("INVALID_NAME"));
        };
        let Some(pending_for) = pending_for(&op.pending_for) else {
            return Ok(rejected("INVALID_NAME"));
        };
        if let Some(existing) = self.st.nodes.get(&op.node_id).cloned() {
            return Ok(if self.same_create(&existing, &op.parent_id, &name, None) {
                unchanged(&existing)
            } else {
                rejected("ID_EXISTS")
            });
        }
        if self.purged_replay(&op.node_id, true).is_some()
            || self.held_elsewhere(&op.node_id, &[Field::Place, Field::Status])
        {
            return Ok(superseded());
        }
        if let Some(deleted) =
            self.create_into_deleted(&op.node_id, &op.parent_id, &name, None, pending_for.clone())
        {
            return Ok(deleted);
        }
        let found = match self.parent_row(&op.parent_id)? {
            Ok(found) => found,
            Err(reason) => return Ok(rejected(reason)),
        };
        if found.parent_id.is_none() && is_reserved_at_root(&name) {
            return Ok(rejected("INVALID_NAME"));
        }
        let parent = self
            .parent_for(&op.parent_id)?
            .expect("the parent was found above");
        if let Some(existing) = self
            .live_with_name(&parent.node_id, &name_key(&name), "")
            .filter(|n| n.kind == NodeKind::Dir)
        {
            let (id, existing_name) = (existing.node_id.clone(), existing.name.clone());
            self.remap.insert(op.node_id.clone(), id.clone());
            return Ok(result(op_result::Code::MergedInto, &id, &existing_name));
        }
        let (name, renamed) = self.pick_name(&parent.node_id, &name, "");
        let dev = self.dev().to_string();
        self.insert_node(
            &op.node_id,
            &parent.node_id,
            &name,
            NodeKind::Dir,
            None,
            &dev,
        );
        self.set_incoming(&op.node_id, Field::Place);
        self.set_incoming(&op.node_id, Field::Status);
        self.mark(&op.node_id, pending_for);
        Ok(applied(renamed, &name))
    }

    fn create_file(&mut self, op: &CreateFile) -> Result<OpResult, Error> {
        let Some(name) = normalize_name(&op.name) else {
            return Ok(rejected("INVALID_NAME"));
        };
        let content = op.content.as_ref().expect("checked before applying");
        if let Some(existing) = self.st.nodes.get(&op.node_id).cloned() {
            return Ok(
                if self.same_create(&existing, &op.parent_id, &name, Some(content)) {
                    unchanged(&existing)
                } else {
                    rejected("ID_EXISTS")
                },
            );
        }
        let bases = self.made_over(&op.node_id, &op.made_over, "");
        let lineage = || chain(&op.source_version_id, &bases);
        if let Some(tombstone) = self.purged_replay(&op.node_id, true) {
            let intent = self.incoming(Field::Content).intent.clone();
            if tombstone.knows(intent.id) || self.held_elsewhere(&op.node_id, &[Field::Content]) {
                return Ok(superseded());
            }
            let home = self.nearest_live(tombstone.parent_id.as_deref());
            return Ok(self.conflict_copy(
                &home,
                &name,
                content,
                &content.chunks,
                lineage(),
                intent,
            ));
        }
        if self.held_elsewhere(&op.node_id, &FIELDS) {
            return Ok(superseded());
        }
        if let Some(deleted) = self.create_into_deleted(
            &op.node_id,
            &op.parent_id,
            &name,
            Some((content, lineage())),
            None,
        ) {
            return Ok(deleted);
        }
        let found = match self.parent_row(&op.parent_id)? {
            Ok(found) => found,
            Err(reason) => return Ok(rejected(reason)),
        };
        if found.parent_id.is_none() && is_reserved_at_root(&name) {
            return Ok(rejected("INVALID_NAME"));
        }
        let parent = self
            .parent_for(&op.parent_id)?
            .expect("the parent was found above");
        let (name, renamed) = self.pick_name(&parent.node_id, &name, "");
        let version_id = self.insert_version(&op.node_id, content, &content.chunks);
        self.set_lineage(&version_id, lineage());
        let dev = self.dev().to_string();
        self.insert_node(
            &op.node_id,
            &parent.node_id,
            &name,
            NodeKind::File,
            Some(version_id),
            &dev,
        );
        for field in FIELDS {
            self.set_incoming(&op.node_id, field);
        }
        if self.ctx.copies_created_files {
            let copy = self.st.ids.uuid_v7(self.ctx.now_ms as u64);
            let copy_name = self.pick_conflict_copy_name(&parent.node_id, &name);
            let version_id = self.insert_version(&copy, content, &content.chunks);
            self.insert_node(
                &copy,
                &parent.node_id,
                &copy_name,
                NodeKind::File,
                Some(version_id),
                &dev,
            );
            for field in FIELDS {
                let intent = self.minted(&dev, false);
                self.set_intent(&copy, field, intent);
            }
        }
        Ok(applied(renamed, &name))
    }

    fn update_content(
        &mut self,
        node_id: &str,
        content: &Content,
        chunks: &[ChunkRef],
        resend: bool,
        made_over: &[String],
        source_version_id: &str,
    ) -> Result<OpResult, Error> {
        let intent = self.incoming(Field::Content).intent.clone();
        let replay = resend || (self.client() && self.incoming(Field::Content).replay);
        let source = if replay { source_version_id } else { "" };
        let Some(found) = self.node(node_id) else {
            let id = self.id(node_id);
            let Some(tombstone) = self.purged_replay(&id, false) else {
                return Ok(rejected("NOT_FOUND"));
            };
            if tombstone.knows(intent.id) || self.held_elsewhere(&id, &[Field::Content]) {
                return Ok(superseded());
            }
            let home = self.nearest_live(tombstone.parent_id.as_deref());
            return Ok(self.conflict_copy(
                &home,
                &tombstone.name,
                content,
                chunks,
                chain(source, made_over),
                intent,
            ));
        };
        if found.kind != NodeKind::File {
            return Ok(rejected("NOT_FILE"));
        }
        if self.client()
            && self.holds_intents(&found.node_id, &[Field::Content])
            && self.holds(&found, content)
        {
            return Ok(unchanged(&found));
        }
        if self.held_elsewhere(&found.node_id, &[Field::Content]) {
            return Ok(superseded());
        }
        let yields = resend || (self.client() && intent.carried);
        let current = found.cur_version_id.clone();
        if !source.is_empty()
            && current.as_ref().is_some_and(|current| {
                self.st.versions.values().any(|version| {
                    version.node_id == node_id
                        && ((version.version_id == source && &version.version_id != current)
                            || version.made_over.iter().any(|id| id == source))
                })
            })
        {
            return Ok(superseded());
        }
        if found.status == NodeStatus::Trashed {
            if self.client() && intent.carried {
                if self.trash_knows(&found.node_id, intent.id) {
                    return Ok(superseded());
                }
                let home = self.nearest_live(found.parent_id.as_deref());
                return Ok(self.conflict_copy(
                    &home,
                    &found.name,
                    content,
                    chunks,
                    chain(source, made_over),
                    intent,
                ));
            }
            if resend {
                return Ok(superseded());
            }
        }
        let bases = self.made_over(node_id, made_over, "");
        let alias = match &current {
            Some(id) if self.client() => self.st.versions[id].source_version_id.clone(),
            _ => String::new(),
        };
        let via_alias = current
            .as_ref()
            .is_some_and(|id| !alias.is_empty() && !bases.contains(id) && bases.contains(&alias));
        let over_current = via_alias || current.as_ref().is_some_and(|id| bases.contains(id));
        let by_ancestry = self.client() && !bases.is_empty();
        let known: Vec<String> = (current.iter())
            .flat_map(|id| {
                std::iter::once(id.clone()).chain(self.st.versions[id].made_over.iter().cloned())
            })
            .collect();
        let merges = by_ancestry
            && current.is_some()
            && !over_current
            && self.holds(&found, content)
            && bases.iter().any(|id| !known.contains(id));
        let stale_content = if by_ancestry {
            !over_current
        } else {
            self.unseen_content(&found)
        };
        if !merges && stale_content && self.holds(&found, content) {
            if !source.is_empty()
                && let Some(id) = &current
            {
                let version = self.st.versions.get_mut(id).unwrap();
                let mut chain = vec![source.to_owned()];
                for id in &version.made_over {
                    if !chain.contains(id) && chain.len() < 64 {
                        chain.push(id.clone());
                    }
                }
                if version.made_over != chain {
                    version.made_over = chain;
                    self.touch(&found.node_id);
                }
            }
            if yields || found.edit_rev >= found.content_rev {
                if self.client() {
                    self.touch(&found.node_id);
                }
                return Ok(applied(false, &found.name));
            }
            let (node, renamed) = self.revive_target(found);
            let (seq, dev) = (self.ctx.seq, self.dev().to_string());
            let stored = self
                .st
                .nodes
                .get_mut(&node.node_id)
                .expect("updated node exists");
            stored.edit_rev = seq;
            stored.edit_dev = dev;
            self.touch(&node.node_id);
            return Ok(applied(renamed, &node.name));
        }
        let lineage = match merges || via_alias {
            true => chain(source, &[bases, known].concat()),
            false => chain(source, &bases),
        };
        let (node, renamed) = self.revive_target(found);
        let stale = !merges
            && if by_ancestry {
                !over_current
            } else {
                self.unseen_content(&node) && !over_current
            };
        if stale {
            let parent =
                self.st.nodes[node.parent_id.as_ref().expect("a file has a parent")].clone();
            return Ok(self.conflict_copy(&parent, &node.name, content, chunks, lineage, intent));
        }
        let version_id = self.insert_version(&node.node_id, content, chunks);
        self.set_lineage(&version_id, lineage);
        let (seq, dev) = (self.ctx.seq, self.dev().to_string());
        let stored = self
            .st
            .nodes
            .get_mut(&node.node_id)
            .expect("updated node exists");
        stored.cur_version_id = Some(version_id);
        if !yields {
            stored.edit_rev = seq;
            stored.edit_dev = dev.clone();
        }
        stored.content_rev = seq;
        stored.content_dev = dev;
        self.set_intent(&node.node_id, Field::Content, intent);
        self.touch(&node.node_id);
        Ok(applied(renamed, &node.name))
    }

    fn is_self_or_descendant(&self, candidate: &Node, ancestor_id: &str) -> bool {
        let mut cur = Some(candidate);
        while let Some(n) = cur {
            if n.node_id == ancestor_id {
                return true;
            }
            cur = n.parent_id.as_ref().and_then(|id| self.st.nodes.get(id));
        }
        false
    }

    fn move_node(&mut self, op: &Move) -> Result<OpResult, Error> {
        let Some(found) = self.node(&op.node_id) else {
            return Ok(match self.purged_replay(&self.id(&op.node_id), false) {
                Some(_) => superseded(),
                None => rejected("NOT_FOUND"),
            });
        };
        if found.parent_id.is_none() {
            return Ok(rejected("ROOT"));
        }
        let Some(name) = normalize_name(&op.new_name) else {
            return Ok(rejected("INVALID_NAME"));
        };
        let Some(pending_for) = pending_for(&op.pending_for) else {
            return Ok(rejected("INVALID_NAME"));
        };
        let Some(target) = self.place_in(&op.new_parent_id) else {
            return Ok(rejected("NOT_FOUND"));
        };
        if target.kind != NodeKind::Dir {
            return Ok(rejected("PARENT_NOT_DIR"));
        }
        let live = found.status == NodeStatus::Live;
        let in_place =
            found.parent_id.as_deref() == Some(target.node_id.as_str()) && found.name == name;
        if self.client() {
            if in_place && self.holds_intents(&found.node_id, &[Field::Place]) {
                return Ok(unchanged(&found));
            }
            if self.held_elsewhere(&found.node_id, &[Field::Place])
                || !self.over_held(&found.node_id, Field::Place)
            {
                return Ok(superseded());
            }
            if (!live && self.incoming(Field::Place).intent.carried)
                || self.deleted_parent(&op.new_parent_id).is_some()
            {
                return Ok(superseded());
            }
        }
        if self.is_self_or_descendant(&target, &found.node_id) {
            return Ok(rejected("CYCLE"));
        }
        if target.parent_id.is_none() && is_reserved_at_root(&name) {
            return Ok(rejected("INVALID_NAME"));
        }
        if let Some(from) = &op.from
            && !self.is_at(&found, from, &target)
        {
            return Ok(superseded());
        }
        let yields =
            op.from.is_some() || (self.client() && self.incoming(Field::Place).intent.carried);
        if live && in_place {
            if yields {
                return Ok(result(op_result::Code::Applied, "", &name));
            }
            let authored = found.meta_edit_rev < found.meta_rev;
            if authored || self.unseen_parent(&found) {
                let (seq, dev) = (self.ctx.seq, self.dev().to_string());
                let stored = self
                    .st
                    .nodes
                    .get_mut(&found.node_id)
                    .expect("moved node exists");
                stored.parent_rev = seq;
                stored.parent_dev = dev.clone();
                if authored {
                    stored.meta_edit_rev = seq;
                    stored.meta_edit_dev = dev;
                    self.set_incoming(&found.node_id, Field::Place);
                    self.touch(&found.node_id);
                }
            }
            return Ok(result(op_result::Code::Applied, "", &name));
        }
        let parent = self
            .parent_for(&target.node_id)?
            .expect("the target is an existing directory");
        self.check_subtree_depth(&found, &parent)?;
        if !live {
            let group = found
                .trash_group
                .clone()
                .expect("a trashed node has a trash group");
            let dev = self.dev().to_string();
            let (name, renamed) = self.revive_one(&found.node_id, &parent.node_id, &name, &dev);
            self.set_incoming(&found.node_id, Field::Place);
            self.revive_group(&found.node_id, &group);
            self.mark(&found.node_id, pending_for);
            return Ok(applied(renamed, &name));
        }
        let (name, renamed) = self.pick_name(&parent.node_id, &name, &found.node_id);
        let placed = found.parent_id.as_deref() != Some(parent.node_id.as_str())
            || (!yields && self.unseen_parent(&found));
        let (seq, dev) = (self.ctx.seq, self.dev().to_string());
        let stored = self
            .st
            .nodes
            .get_mut(&found.node_id)
            .expect("moved node exists");
        if placed {
            stored.parent_rev = seq;
            stored.parent_dev = dev.clone();
        }
        stored.parent_id = Some(parent.node_id.clone());
        stored.name_key = name_key(&name);
        stored.name = name.clone();
        stored.meta_rev = seq;
        if !yields {
            stored.meta_edit_rev = seq;
            stored.meta_edit_dev = dev.clone();
        }
        stored.meta_dev = dev;
        self.set_incoming(&found.node_id, Field::Place);
        self.mark(&found.node_id, pending_for);
        self.touch(&found.node_id);
        Ok(applied(renamed, &name))
    }

    fn is_at(&self, node: &Node, from: &Place, target: &Node) -> bool {
        node.status == NodeStatus::Live
            && target.status == NodeStatus::Live
            && node.parent_id.as_deref() == Some(self.id(&from.parent_id).as_str())
            && normalize_name(&from.name).is_some_and(|name| node.name == name)
    }

    fn trash_node(&mut self, node_id: &str, group: &str) {
        let (seq, dev, now_ms) = (self.ctx.seq, self.dev().to_string(), self.ctx.now_ms);
        let stored = self.st.nodes.get_mut(node_id).expect("trashed node exists");
        stored.status_before = stored.intents.id(Field::Status);
        stored.status = NodeStatus::Trashed;
        stored.trash_group = Some(group.to_string());
        stored.trashed_at_ms = Some(now_ms);
        stored.meta_rev = seq;
        stored.meta_edit_rev = seq;
        stored.meta_edit_dev = dev.clone();
        stored.meta_dev = dev;
        self.set_incoming(node_id, Field::Status);
        self.report(node_id);
    }

    fn trash_subtree(&mut self, node: &Node, group: &str) -> bool {
        if self.unseen_parent(node) {
            return true;
        }
        let mut keep = self.unseen_edit(node) || self.unseen_meta(node);
        for child in self.st.children(&node.node_id, NodeStatus::Live) {
            keep = self.trash_subtree(&child, group) || keep;
        }
        if keep {
            return true;
        }
        self.trash_node(&node.node_id, group);
        false
    }

    fn in_view(view: &DeleteView, intent: &FieldIntent) -> bool {
        view.held.contains(&intent.id)
            || (intent.seq <= view.base_seq && !view.unseen.contains(&intent.id))
    }

    fn unseen_by(&self, view: &DeleteView, intent: Option<&FieldIntent>) -> bool {
        intent.is_some_and(|intent| {
            !intent.carried
                && !Self::in_view(view, intent)
                && (view.unseen.contains(&intent.id)
                    || (intent.author != self.dev() && intent.seq > view.base_seq))
        })
    }

    fn walk_view(
        &mut self,
        node: &Node,
        target_id: &str,
        group: &str,
        view: &mut DeleteView,
        lost: &mut Vec<LostContent>,
    ) -> bool {
        let held = self.st.nodes[&node.node_id].intents.clone();
        if let Some(place) = held.get(Field::Place)
            && self.unseen_by(view, Some(place))
            && node.parent_rev >= place.seq
        {
            return true;
        }
        let mut keep = FIELDS
            .iter()
            .any(|&field| self.unseen_by(view, held.get(field)));
        for child in self.st.children(&node.node_id, NodeStatus::Live) {
            keep = self.walk_view(&child, target_id, group, view, lost) || keep;
        }
        if keep {
            return true;
        }
        if let Some(content) = held.get(Field::Content) {
            let seen = Self::in_view(view, content);
            if seen && !view.seen_content.contains(&content.id) {
                view.seen_content.push(content.id);
            }
            let place = held.get(Field::Place).filter(|place| place.carried);
            if node.kind == NodeKind::File && !seen && (place.is_some() || content.carried) {
                lost.push(LostContent {
                    node: node.clone(),
                    content: content.clone(),
                    place_author: place.map(|place| place.author.clone()),
                });
                if place.is_some() {
                    return node.node_id == target_id;
                }
            }
        }
        self.trash_node(&node.node_id, group);
        false
    }

    fn trash_by_view(&mut self, target: &Node, group: &str, view: &mut DeleteView) -> bool {
        let mut lost = Vec::new();
        let kept = self.walk_view(target, &target.node_id, group, view, &mut lost);
        for LostContent {
            node,
            content,
            place_author,
        } in lost
        {
            let Some(author) = place_author else {
                let home = self.nearest_live(node.parent_id.as_deref());
                self.copy_of(&node, &home, content);
                continue;
            };
            let parent_live = (node.parent_id.as_ref())
                .and_then(|id| self.st.nodes.get(id))
                .is_some_and(|parent| parent.status == NodeStatus::Live);
            if node.node_id == target.node_id || parent_live {
                continue;
            }
            let parent = self.nearest_live(node.parent_id.as_deref());
            let (name, _) = self.pick_name(&parent.node_id, &node.name, &node.node_id);
            let (seq, dev) = (self.ctx.seq, self.dev().to_string());
            let stored = self
                .st
                .nodes
                .get_mut(&node.node_id)
                .expect("a kept copy exists");
            stored.parent_id = Some(parent.node_id.clone());
            stored.parent_rev = seq;
            stored.parent_dev = String::new();
            stored.name_key = name_key(&name);
            stored.name = name;
            stored.meta_rev = seq;
            stored.meta_dev = dev;
            let place = self.minted(&author, true);
            self.set_intent(&node.node_id, Field::Place, place);
            self.touch(&node.node_id);
        }
        kept
    }

    fn trash_copies(&mut self, ids: &[IntentId], group: &str) {
        for &id in ids {
            for (holder, field) in self.st.holders(id) {
                let copy = &self.st.nodes[&holder];
                let marked = copy.copy_mark.is_some_and(|mark| {
                    Some(mark)
                        == copy
                            .intents
                            .id(Field::Place)
                            .zip(copy.intents.id(Field::Status))
                });
                if field == Field::Content && copy.status == NodeStatus::Live && marked {
                    self.trash_node(&holder, group);
                }
            }
        }
    }

    fn merge_held(&mut self, node: &Node, held: &[IntentId]) {
        let group = node
            .trash_group
            .clone()
            .expect("a trashed node has a trash group");
        let stored = self.st.trash_held.entry(group.clone()).or_default();
        let before = stored.len();
        stored.extend(held.iter().copied());
        let added = stored.len() > before;
        if stored.is_empty() {
            self.st.trash_held.remove(&group);
        }
        if added {
            let (seq, dev) = (self.ctx.seq, self.dev().to_string());
            let mut members: Vec<&mut Node> = (self.st.nodes.values_mut())
                .filter(|n| n.trash_group.as_deref() == Some(group.as_str()))
                .collect();
            members.sort_by_key(|n| n.row);
            let ids: Vec<String> = members
                .into_iter()
                .map(|member| {
                    member.meta_rev = seq;
                    member.meta_dev = dev.clone();
                    member.node_id.clone()
                })
                .collect();
            for id in ids {
                self.touch(&id);
            }
        }
        self.trash_copies(held, &group);
    }

    fn delete(&mut self, op: &Delete, index: usize) -> OpResult {
        let Some(node) = self.node(&op.node_id) else {
            return match self.purged_replay(&self.id(&op.node_id), false) {
                Some(_) => superseded(),
                None => rejected("NOT_FOUND"),
            };
        };
        if node.parent_id.is_none() {
            return rejected("ROOT");
        }
        let mut held: Vec<IntentId> = Vec::new();
        for id in op.held.iter().filter_map(|id| IntentId::from_wire(id)) {
            if !held.contains(&id) {
                held.push(id);
            }
        }
        let trashed = node.status == NodeStatus::Trashed;
        if self.client() {
            if trashed && self.holds_intents(&node.node_id, &[Field::Status]) {
                self.merge_held(&node, &held);
                return unchanged(&node);
            }
            if trashed && self.intents.over.is_some() && node.status_before == self.intents.over {
                self.merge_held(&node, &held);
                return result(op_result::Code::Applied, "", "");
            }
            if !self.over_held(&node.node_id, Field::Status) {
                return superseded();
            }
        }
        if trashed {
            if self.client() {
                self.merge_held(&node, &held);
            }
            return result(op_result::Code::Applied, "", "");
        }
        let required = match &op.requires {
            None => None,
            Some(required) => match self.node(&required.node_id) {
                Some(kept)
                    if kept.status == NodeStatus::Live
                        && kept.content_rev == required.content_rev
                        && kept.meta_rev == required.meta_rev =>
                {
                    Some(kept.node_id)
                }
                _ => return superseded(),
            },
        };
        let group = (self.joined_group(&op.trash_group, index))
            .unwrap_or_else(|| format!("{}:{index}", self.ctx.commit_id));
        if self.client() {
            let mut view = DeleteView {
                base_seq: self.ctx.base_seq,
                unseen: (op.unseen.iter())
                    .filter_map(|id| IntentId::from_wire(id))
                    .collect(),
                held: held.clone(),
                seen_content: Vec::new(),
            };
            let in_group = |st: &State| {
                (st.nodes.values())
                    .filter(|n| n.trash_group.as_deref() == Some(group.as_str()))
                    .count()
            };
            let before = in_group(self.st);
            let kept = self.trash_by_view(&node, &group, &mut view);
            if in_group(self.st) == before {
                return superseded();
            }
            if !held.is_empty() {
                self.st
                    .trash_held
                    .entry(group.clone())
                    .or_default()
                    .extend(held.iter().copied());
            }
            let seen: Vec<IntentId> = view.held.into_iter().chain(view.seen_content).collect();
            self.trash_copies(&seen, &group);
            if kept {
                return superseded();
            }
        } else if self.trash_subtree(&node, &group) {
            return superseded();
        }
        if let Some(required) = required {
            let (seq, dev) = (self.ctx.seq, self.dev().to_string());
            let stored = self
                .st
                .nodes
                .get_mut(&required)
                .expect("required node exists");
            stored.parent_rev = seq;
            stored.parent_dev = dev;
        }
        result(op_result::Code::Applied, "", "")
    }

    fn joined_group(&self, group: &str, index: usize) -> Option<String> {
        let (commit_id, at) = group.rsplit_once(':')?;
        let at: usize = at.parse().ok()?;
        let joins = match commit_id == self.ctx.commit_id {
            true => at <= index,
            false => {
                let own = (self.ctx.results.get(commit_id))
                    .is_some_and(|stored| stored.device_id == self.ctx.committer.device_id);
                own && (self.st.nodes.values()).any(|node| {
                    node.status == NodeStatus::Trashed && node.trash_group.as_deref() == Some(group)
                })
            }
        };
        (joins && format!("{commit_id}:{at}") == group).then(|| group.to_owned())
    }

    fn restore(&mut self, op: &Restore) -> Result<OpResult, Error> {
        let Some(node) = self.node(&op.node_id) else {
            return Ok(match self.purged_replay(&self.id(&op.node_id), false) {
                Some(_) => superseded(),
                None => rejected("NOT_FOUND"),
            });
        };
        if !op.version_id.is_empty() {
            let Some(version) = self
                .st
                .versions
                .get(&op.version_id)
                .filter(|v| v.node_id == node.node_id)
                .cloned()
            else {
                return Ok(rejected("NOT_FOUND"));
            };
            let chunks = version.content.chunks.clone();
            return self.update_content(
                &node.node_id,
                &Content {
                    chunks: Vec::new(),
                    ..version.content
                },
                &chunks,
                false,
                &[],
                "",
            );
        }
        let Some(original_parent) = node.parent_id.clone() else {
            return Ok(rejected("ROOT"));
        };
        if node.status != NodeStatus::Trashed {
            return Ok(result(op_result::Code::Applied, "", &node.name));
        }
        if self.client() && self.incoming(Field::Status).intent.carried {
            return Ok(superseded());
        }
        let name = if op.name.is_empty() {
            Some(node.name.clone())
        } else {
            normalize_name(&op.name)
        };
        let Some(name) = name else {
            return Ok(rejected("INVALID_NAME"));
        };
        let target_id = if op.target_parent_id.is_empty() {
            original_parent
        } else {
            op.target_parent_id.clone()
        };
        let Some(target) = self.node(&target_id) else {
            return Ok(rejected("NOT_FOUND"));
        };
        if target.kind != NodeKind::Dir {
            return Ok(rejected("PARENT_NOT_DIR"));
        }
        if self.is_self_or_descendant(&target, &node.node_id) {
            return Ok(rejected("CYCLE"));
        }
        if target.parent_id.is_none() && is_reserved_at_root(&name) {
            return Ok(rejected("INVALID_NAME"));
        }
        let parent = self
            .parent_for(&target.node_id)?
            .expect("the target is an existing directory");
        self.check_subtree_depth(&node, &parent)?;
        let group = node
            .trash_group
            .clone()
            .expect("a trashed node has a trash group");
        let dev = self.dev().to_string();
        let (name, renamed) = self.revive_one(&node.node_id, &parent.node_id, &name, &dev);
        self.revive_group(&node.node_id, &group);
        if !op.name.is_empty() || !op.target_parent_id.is_empty() {
            self.mark(&node.node_id, None);
        }
        Ok(applied(renamed, &name))
    }
}

fn pending_for(name: &str) -> Option<Option<String>> {
    match name {
        "" => Some(None),
        name => normalize_name(name).map(Some),
    }
}
