// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeSet, HashMap};

use koidrive_engine::names::{conflict_name, is_reserved_at_root, name_key, normalize_name};
use koidrive_engine::tree::same_content;
use koidrive_proto::v1::{
    ChangeRecord, ChunkRef, Content, CreateDir, CreateFile, Delete, Error, Move, NodeKind,
    NodeStatus, Op, OpResult, Place, Restore, change_record, error, op, op_result,
};

use super::{ChunkState, Node, Session, State, Version, fail};

pub const MAX_DEPTH: usize = 256;

pub(super) struct Context<'a> {
    pub seq: u64,
    pub commit_id: &'a str,
    pub base_seq: u64,
    pub now_ms: i64,
    pub committer: &'a Session,
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

fn applied(renamed: bool, name: &str) -> OpResult {
    let code = if renamed {
        op_result::Code::AppliedRenamed
    } else {
        op_result::Code::Applied
    };
    result(code, "", name)
}

pub(super) struct Applier<'a> {
    st: &'a mut State,
    ctx: Context<'a>,
    remap: HashMap<String, String>,
    touched: Vec<String>,
    touched_set: BTreeSet<String>,
}

impl<'a> Applier<'a> {
    pub fn new(st: &'a mut State, ctx: Context<'a>) -> Self {
        Self {
            st,
            ctx,
            remap: HashMap::new(),
            touched: Vec::new(),
            touched_set: BTreeSet::new(),
        }
    }

    pub fn apply(mut self, ops: &[Op]) -> Result<(Vec<OpResult>, Vec<ChangeRecord>), Error> {
        let mut results = Vec::with_capacity(ops.len());
        for (index, op) in ops.iter().enumerate() {
            results.push(self.apply_op(op, index)?);
        }
        let changes = self
            .touched
            .iter()
            .map(|id| ChangeRecord {
                change: Some(change_record::Change::Node(
                    self.st.node_state(&self.st.nodes[id]),
                )),
            })
            .collect();
        Ok((results, changes))
    }

    fn apply_op(&mut self, op: &Op, index: usize) -> Result<OpResult, Error> {
        match &op.op {
            Some(op::Op::CreateDir(op)) => self.create_dir(op),
            Some(op::Op::CreateFile(op)) => self.create_file(op),
            Some(op::Op::UpdateContent(op)) => {
                let content = op.content.as_ref().expect("checked before applying");
                self.update_content(&op.node_id, content, &content.chunks, op.resend)
            }
            Some(op::Op::Move(op)) => self.move_node(op),
            Some(op::Op::Delete(op)) => Ok(self.delete(op, index)),
            Some(op::Op::Restore(op)) => self.restore(op),
            None => Err(fail(
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
        node.status = NodeStatus::Live;
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
            self.revive_one(
                &n.node_id,
                n.parent_id.as_deref().expect("a trashed node has a parent"),
                &n.name,
                "",
            );
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

    fn parent_for(&mut self, parent_id: &str) -> Result<Result<Node, Reject>, Error> {
        let Some(parent) = self.node(parent_id) else {
            return Ok(Err("NOT_FOUND"));
        };
        if parent.kind != NodeKind::Dir {
            return Ok(Err("PARENT_NOT_DIR"));
        }
        self.check_depth(&parent)?;
        if parent.status == NodeStatus::Trashed {
            self.revive_chain(&parent.node_id);
        }
        Ok(Ok(parent))
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
            row,
        };
        self.st.nodes.insert(node_id.into(), node);
        self.touch(node_id);
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

    fn create_dir(&mut self, op: &CreateDir) -> Result<OpResult, Error> {
        let Some(name) = normalize_name(&op.name) else {
            return Ok(rejected("INVALID_NAME"));
        };
        let Some(pending_for) = pending_for(&op.pending_for) else {
            return Ok(rejected("INVALID_NAME"));
        };
        if self.st.nodes.contains_key(&op.node_id) {
            return Ok(rejected("ID_EXISTS"));
        }
        let parent = match self.parent_for(&op.parent_id)? {
            Ok(parent) => parent,
            Err(reason) => return Ok(rejected(reason)),
        };
        if parent.parent_id.is_none() && is_reserved_at_root(&name) {
            return Ok(rejected("INVALID_NAME"));
        }
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
        self.mark(&op.node_id, pending_for);
        Ok(applied(renamed, &name))
    }

    fn create_file(&mut self, op: &CreateFile) -> Result<OpResult, Error> {
        let Some(name) = normalize_name(&op.name) else {
            return Ok(rejected("INVALID_NAME"));
        };
        if self.st.nodes.contains_key(&op.node_id) {
            return Ok(rejected("ID_EXISTS"));
        }
        let parent = match self.parent_for(&op.parent_id)? {
            Ok(parent) => parent,
            Err(reason) => return Ok(rejected(reason)),
        };
        if parent.parent_id.is_none() && is_reserved_at_root(&name) {
            return Ok(rejected("INVALID_NAME"));
        }
        let (name, renamed) = self.pick_name(&parent.node_id, &name, "");
        let content = op.content.as_ref().expect("checked before applying");
        let version_id = self.insert_version(&op.node_id, content, &content.chunks);
        let dev = self.dev().to_string();
        self.insert_node(
            &op.node_id,
            &parent.node_id,
            &name,
            NodeKind::File,
            Some(version_id),
            &dev,
        );
        Ok(applied(renamed, &name))
    }

    fn update_content(
        &mut self,
        node_id: &str,
        content: &Content,
        chunks: &[ChunkRef],
        resend: bool,
    ) -> Result<OpResult, Error> {
        let Some(found) = self.node(node_id) else {
            return Ok(rejected("NOT_FOUND"));
        };
        if found.kind != NodeKind::File {
            return Ok(rejected("NOT_FILE"));
        }
        if resend && found.status == NodeStatus::Trashed {
            return Ok(result(op_result::Code::Superseded, "", ""));
        }
        let current = (found.cur_version_id.as_ref()).map(|id| &self.st.versions[id].content);
        if self.unseen_content(&found)
            && current.is_some_and(|current| same_content(current, content))
        {
            if resend || found.edit_rev >= found.content_rev {
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
        let (node, renamed) = self.revive_target(found);
        let parent_id = node.parent_id.clone().expect("a file has a parent");
        if self.unseen_content(&node) {
            let copy_id = self.st.ids.uuid_v7(self.ctx.now_ms as u64);
            let name = self.pick_conflict_copy_name(&parent_id, &node.name);
            let version_id = self.insert_version(&copy_id, content, chunks);
            self.insert_node(
                &copy_id,
                &parent_id,
                &name,
                NodeKind::File,
                Some(version_id),
                "",
            );
            return Ok(result(op_result::Code::ConflictCopy, &copy_id, &name));
        }
        let version_id = self.insert_version(&node.node_id, content, chunks);
        let (seq, dev) = (self.ctx.seq, self.dev().to_string());
        let stored = self
            .st
            .nodes
            .get_mut(&node.node_id)
            .expect("updated node exists");
        stored.cur_version_id = Some(version_id);
        if !resend {
            stored.edit_rev = seq;
            stored.edit_dev = dev.clone();
        }
        stored.content_rev = seq;
        stored.content_dev = dev;
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
            return Ok(rejected("NOT_FOUND"));
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
        let Some(target) = self.node(&op.new_parent_id) else {
            return Ok(rejected("NOT_FOUND"));
        };
        if target.kind != NodeKind::Dir {
            return Ok(rejected("PARENT_NOT_DIR"));
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
            return Ok(result(op_result::Code::Superseded, "", ""));
        }
        if found.status == NodeStatus::Live
            && found.parent_id.as_deref() == Some(target.node_id.as_str())
            && found.name == name
        {
            if op.from.is_some() {
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
                    self.touch(&found.node_id);
                }
            }
            return Ok(result(op_result::Code::Applied, "", &name));
        }
        let parent = self
            .parent_for(&target.node_id)?
            .expect("the target is an existing directory");
        self.check_subtree_depth(&found, &parent)?;
        if found.status == NodeStatus::Trashed {
            let group = found
                .trash_group
                .clone()
                .expect("a trashed node has a trash group");
            let dev = self.dev().to_string();
            let (name, renamed) = self.revive_one(&found.node_id, &parent.node_id, &name, &dev);
            self.revive_group(&found.node_id, &group);
            self.mark(&found.node_id, pending_for);
            return Ok(applied(renamed, &name));
        }
        let (name, renamed) = self.pick_name(&parent.node_id, &name, &found.node_id);
        let placed = found.parent_id.as_deref() != Some(parent.node_id.as_str())
            || (op.from.is_none() && self.unseen_parent(&found));
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
        if op.from.is_none() {
            stored.meta_edit_rev = seq;
            stored.meta_edit_dev = dev.clone();
        }
        stored.meta_dev = dev;
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
        let (seq, dev) = (self.ctx.seq, self.dev().to_string());
        let stored = self
            .st
            .nodes
            .get_mut(&node.node_id)
            .expect("trashed node exists");
        stored.status = NodeStatus::Trashed;
        stored.trash_group = Some(group.to_string());
        stored.meta_rev = seq;
        stored.meta_edit_rev = seq;
        stored.meta_edit_dev = dev.clone();
        stored.meta_dev = dev;
        self.touch(&node.node_id);
        false
    }

    fn delete(&mut self, op: &Delete, index: usize) -> OpResult {
        let Some(node) = self.node(&op.node_id) else {
            return rejected("NOT_FOUND");
        };
        if node.parent_id.is_none() {
            return rejected("ROOT");
        }
        if node.status == NodeStatus::Trashed {
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
                _ => return result(op_result::Code::Superseded, "", ""),
            },
        };
        if self.trash_subtree(&node, &format!("{}:{index}", self.ctx.commit_id)) {
            return result(op_result::Code::Superseded, "", "");
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

    fn restore(&mut self, op: &Restore) -> Result<OpResult, Error> {
        let Some(node) = self.node(&op.node_id) else {
            return Ok(rejected("NOT_FOUND"));
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
            );
        }
        let Some(original_parent) = node.parent_id.clone() else {
            return Ok(rejected("ROOT"));
        };
        if node.status != NodeStatus::Trashed {
            return Ok(result(op_result::Code::Applied, "", &node.name));
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
