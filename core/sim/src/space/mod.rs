// SPDX-License-Identifier: Apache-2.0

//! An in-memory Space DO written from spec v0 sections 6 to 8 and checked against the TypeScript
//! server (koidrive#6): commit idempotency and the commit checks of RFC 0002 section 3 (history fence,
//! device counter, `base_seq`, `require_head`), chunk presence and receipts, rules R1 to R11, trash
//! groups, revisions and post-image change records, resume and the paged listings.
//! Garbage collection and retention are not modelled.

mod apply;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use koidrive_engine::ids::{IdGen, is_uuid_v7};
use koidrive_proto::v1::{
    ChangeRecord, Changes, ChunkPlan, ChunkRef, ChunkTarget, Commit, CommitRecord, CommitResult,
    Content, DownloadTargets, Error, GetDownloadTargets, GetVersionChunks, ListTrash, ListTree,
    ListVersions, NodeKind, NodeState, NodeStatus, Op, PrepareUpload, PrepareUploadResult, Resume,
    ResyncRequired, RpcRequest, RpcResponse, ServerMessage, Target, TrashPage, TreePage,
    UploadReceipt, VersionChunks, VersionInfo, VersionList, Welcome, chunk_plan, error, op,
    resync_required, rpc_request, rpc_response, server_message,
};
use prost::Message;
use sha2::{Digest, Sha256};

use apply::{Applier, Context};

pub use apply::MAX_DEPTH;

pub const FORMAT_VERSION: u32 = 1;
pub const MAX_OPS: usize = 1000;
pub const MAX_CHUNKS_PER_CALL: usize = 1000;
pub const MAX_CHUNK_BYTES: u32 = 16 * 1024 * 1024;
pub const INLINE_CHUNKS: u32 = 16_384;
/// The largest `ServerMessage` (spec v0 section 7.2). A commit whose record, pushed in `Changes`, or
/// whose result, answered in an `RpcResponse`, would not fit in one is refused.
pub const MAX_SERVER_MESSAGE_BYTES: usize = 1024 * 1024;
pub const PAGE_CHUNKS: usize = 4096;
const GRANT_MS: i64 = 3_600_000;
const PAGE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Owner,
    Editor,
    Viewer,
}

/// Who is calling, as the Worker and the WebSocket `Hello` establish it.
#[derive(Clone, Debug)]
pub struct Session {
    pub device_id: String,
    pub user_id: String,
    pub device_name: String,
    pub role: Role,
    pub max_format: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChunkState {
    Pending,
    Live,
}

#[derive(Clone, Debug)]
pub(crate) struct ChunkRow {
    size: u32,
    state: ChunkState,
}

#[derive(Clone, Debug)]
pub(crate) struct Node {
    node_id: String,
    parent_id: Option<String>,
    name: String,
    name_key: String,
    kind: NodeKind,
    status: NodeStatus,
    content_rev: u64,
    content_dev: String,
    edit_rev: u64,
    edit_dev: String,
    meta_rev: u64,
    meta_dev: String,
    meta_edit_rev: u64,
    meta_edit_dev: String,
    /// The seq and device of the commit that last gave the node a new parent or brought it back from
    /// the trash. `parent_dev` is empty unless that commit's op named both the node and the parent.
    parent_rev: u64,
    parent_dev: String,
    cur_version_id: Option<String>,
    trash_group: Option<String>,
    pending_for: Option<String>,
    pending_by: Option<String>,
    /// Insertion order, standing in for SQLite's rowid where the server reads rows without ORDER BY.
    row: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Version {
    version_id: String,
    node_id: String,
    seq: u64,
    /// Every chunk inline, with `chunk_count` set.
    content: Content,
    device_id: String,
    user_id: String,
    created_at_ms: i64,
}

/// Everything one commit can change, cloned per commit so that a failing commit leaves no trace.
#[derive(Clone, Debug)]
pub(crate) struct State {
    nodes: BTreeMap<String, Node>,
    versions: BTreeMap<String, Version>,
    chunks: BTreeMap<Vec<u8>, ChunkRow>,
    rows: u64,
    ids: IdGen,
}

impl State {
    fn next_row(&mut self) -> u64 {
        self.rows += 1;
        self.rows
    }

    fn children(&self, parent_id: &str, status: NodeStatus) -> Vec<Node> {
        let mut children: Vec<Node> = self
            .nodes
            .values()
            .filter(|n| n.parent_id.as_deref() == Some(parent_id) && n.status == status)
            .cloned()
            .collect();
        children.sort_by_key(|n| n.row);
        children
    }

    fn version_info(&self, version: &Version) -> VersionInfo {
        VersionInfo {
            version_id: version.version_id.clone(),
            seq: version.seq,
            content: Some(inline_content(&version.content)),
            device_id: version.device_id.clone(),
            user_id: version.user_id.clone(),
            created_at_ms: version.created_at_ms,
        }
    }

    fn node_state(&self, node: &Node) -> NodeState {
        NodeState {
            node_id: node.node_id.clone(),
            parent_id: node.parent_id.clone().unwrap_or_default(),
            name: node.name.clone(),
            kind: node.kind as i32,
            status: node.status as i32,
            content_rev: node.content_rev,
            edit_rev: node.edit_rev,
            meta_rev: node.meta_rev,
            meta_edit_rev: node.meta_edit_rev,
            trash_group: node.trash_group.clone().unwrap_or_default(),
            pending_for: node.pending_for.clone().unwrap_or_default(),
            pending_by: node.pending_by.clone().unwrap_or_default(),
            current: node
                .cur_version_id
                .as_ref()
                .map(|id| self.version_info(&self.versions[id])),
        }
    }
}

/// A version's content as records carry it: chunks inline up to 16,384, `chunk_count` always.
fn inline_content(content: &Content) -> Content {
    let chunks = if content.chunk_count <= INLINE_CHUNKS {
        content.chunks.clone()
    } else {
        Vec::new()
    };
    Content {
        chunks,
        ..content.clone()
    }
}

pub(crate) fn fail(code: error::Code, message: impl Into<String>) -> Error {
    Error {
        code: code as i32,
        message: message.into(),
        ..Default::default()
    }
}

/// `(32-byte hash || u32 LE size)` for every chunk, the bytes `manifest_digest` hashes (spec v0 section 5.1).
fn encode_chunk_list(chunks: &[ChunkRef]) -> Vec<u8> {
    chunks
        .iter()
        .flat_map(|c| c.hash.iter().copied().chain(c.size.to_le_bytes()))
        .collect()
}

fn check_chunk_ref(chunk: &ChunkRef) -> Result<(), Error> {
    if chunk.hash.len() != 32 {
        return Err(fail(
            error::Code::InvalidArgument,
            "chunk hash must be 32 bytes",
        ));
    }
    if chunk.size < 1 || chunk.size > MAX_CHUNK_BYTES {
        return Err(fail(
            error::Code::InvalidArgument,
            "chunk size out of range",
        ));
    }
    if chunk.encoding != 0 {
        return Err(fail(
            error::Code::InvalidArgument,
            "unsupported chunk encoding",
        ));
    }
    Ok(())
}

fn check_content(content: Option<&Content>) -> Result<&Content, Error> {
    let content =
        content.ok_or_else(|| fail(error::Code::InvalidArgument, "content is required"))?;
    if content.chunks.len() != content.chunk_count as usize {
        return Err(fail(
            error::Code::InvalidArgument,
            "commits must list every chunk inline",
        ));
    }
    content.chunks.iter().try_for_each(check_chunk_ref)?;
    if content
        .chunks
        .iter()
        .map(|c| u64::from(c.size))
        .sum::<u64>()
        != content.size
    {
        return Err(fail(
            error::Code::InvalidArgument,
            "content size does not match its chunks",
        ));
    }
    if Sha256::digest(encode_chunk_list(&content.chunks)).as_slice()
        != content.manifest_digest.as_slice()
    {
        return Err(fail(
            error::Code::InvalidArgument,
            "manifest_digest does not match the chunk list",
        ));
    }
    Ok(content)
}

fn check_ids(op: &Op, index: usize) -> Result<(), Error> {
    let (required, optional): (Vec<&str>, Vec<&str>) = match &op.op {
        Some(op::Op::CreateDir(o)) => (vec![&o.node_id, &o.parent_id], vec![]),
        Some(op::Op::CreateFile(o)) => (vec![&o.node_id, &o.parent_id], vec![]),
        Some(op::Op::UpdateContent(o)) => (vec![&o.node_id], vec![]),
        Some(op::Op::Delete(o)) => (vec![&o.node_id], vec![]),
        Some(op::Op::Move(o)) => (vec![&o.node_id, &o.new_parent_id], vec![]),
        Some(op::Op::Restore(o)) => (vec![&o.node_id], vec![&o.version_id, &o.target_parent_id]),
        None => {
            return Err(fail(
                error::Code::InvalidArgument,
                format!("op {index} is empty"),
            ));
        }
    };
    if !required.iter().all(|id| is_uuid_v7(id))
        || !optional.iter().all(|id| id.is_empty() || is_uuid_v7(id))
    {
        return Err(fail(
            error::Code::InvalidArgument,
            format!("op {index} has a malformed id"),
        ));
    }
    Ok(())
}

fn op_content(op: &Op) -> Option<Option<&Content>> {
    match &op.op {
        Some(op::Op::CreateFile(o)) => Some(o.content.as_ref()),
        Some(op::Op::UpdateContent(o)) => Some(o.content.as_ref()),
        _ => None,
    }
}

const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// JavaScript's `Number(text)` for a string, which the server applies to a version cursor: surrounding
/// JavaScript whitespace is ignored, an empty string is 0, `0x`, `0o` and `0b` prefixes are unsigned
/// integers, and anything else is a signed decimal literal with an optional fraction and exponent.
/// `None` stands for `NaN` and the infinities, which no caller accepts.
fn js_number(text: &str) -> Option<f64> {
    let is_space = |c: char| {
        matches!(
            c,
            '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200A}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202F}'
                    | '\u{205F}'
                    | '\u{3000}'
                    | '\u{FEFF}'
        )
    };
    let text = text.trim_matches(is_space);
    if text.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        let Some(digits) = text
            .get(..2)
            .filter(|head| head.eq_ignore_ascii_case(prefix))
            .and(text.get(2..))
            .filter(|digits| !digits.is_empty())
        else {
            continue;
        };
        if !digits.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        return Some(
            digits
                .chars()
                .filter_map(|c| c.to_digit(radix))
                .fold(0.0, |n, d| n * f64::from(radix) + f64::from(d)),
        );
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let decimal = unsigned.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        && unsigned
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    if !decimal {
        return None;
    }
    text.parse::<f64>().ok().filter(|n| n.is_finite())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// An upload receipt this space issued. The server's token is an HMAC over the space, hash, size and
/// expiry (server/src/crypto.ts); here the token is an opaque handle to this record, so a receipt from
/// another space, for another chunk or past its expiry is refused the same way.
#[derive(Clone, Debug)]
struct IssuedReceipt {
    hash: Vec<u8>,
    size: u32,
    expires_at_ms: i64,
}

/// The outcome of [`SpaceDo::commit`]: the result for the committing device and, for a commit that was
/// applied now rather than found by its `commit_id`, the record to push to the other connections.
#[derive(Clone, Debug, PartialEq)]
pub struct Committed {
    pub result: CommitResult,
    pub record: Option<CommitRecord>,
}

#[derive(Clone, Debug)]
struct StoredResult {
    device_id: String,
    request: Commit,
    result: CommitResult,
}

fn covered(commit: &Commit) -> Commit {
    Commit {
        base_seq: commit.base_seq,
        ops: commit.ops.clone(),
        device_seq: commit.device_seq,
        require_head: commit.require_head,
        ..Commit::default()
    }
}

#[derive(Clone)]
pub struct SpaceDo {
    space_id: String,
    history_id: String,
    format_version: u32,
    state: State,
    log: Vec<CommitRecord>,
    results: HashMap<String, StoredResult>,
    /// The highest accepted `device_seq` per device (RFC 0002 section 2). `results` keeps every
    /// commit's result, so the result of that commit is found there.
    device_seqs: HashMap<String, u64>,
    receipts: HashMap<String, IssuedReceipt>,
    /// The bucket: the size of every chunk object a `PUT` stored, by hash. A commit counts a
    /// `pending` chunk without a valid receipt as uploaded when its object is here with the row's
    /// size, the `head` check of RFC 0002 section 11. Nothing deletes objects, so the object cannot
    /// go between that check and the commit.
    objects: HashMap<Vec<u8>, u32>,
    head_seq: u64,
    restore_seq: u64,
    log_floor_seq: u64,
    /// The commit token at `log_floor_seq` (RFC 0013), kept when the log is truncated past it.
    floor_token: Option<(String, i64)>,
}

impl SpaceDo {
    /// A new space whose IDs, including the space and history IDs, come from `seed`.
    pub fn new(seed: u64, now_ms: i64) -> Self {
        let mut ids = IdGen::new(seed);
        let space_id = ids.uuid_v7(now_ms as u64);
        let history_id = ids.uuid_v7(now_ms as u64);
        let root = Node {
            node_id: space_id.clone(),
            parent_id: None,
            name: String::new(),
            name_key: String::new(),
            kind: NodeKind::Dir,
            status: NodeStatus::Live,
            content_rev: 0,
            content_dev: String::new(),
            edit_rev: 0,
            edit_dev: String::new(),
            meta_rev: 0,
            meta_dev: String::new(),
            meta_edit_rev: 0,
            meta_edit_dev: String::new(),
            parent_rev: 0,
            parent_dev: String::new(),
            cur_version_id: None,
            trash_group: None,
            pending_for: None,
            pending_by: None,
            row: 1,
        };
        Self {
            history_id,
            format_version: FORMAT_VERSION,
            state: State {
                nodes: BTreeMap::from([(space_id.clone(), root)]),
                versions: BTreeMap::new(),
                chunks: BTreeMap::new(),
                rows: 1,
                ids,
            },
            space_id,
            log: Vec::new(),
            results: HashMap::new(),
            device_seqs: HashMap::new(),
            receipts: HashMap::new(),
            objects: HashMap::new(),
            head_seq: 0,
            restore_seq: 0,
            log_floor_seq: 0,
            floor_token: None,
        }
    }

    /// The commit token at `seq` (RFC 0013): the `commit_id` and `committed_at_ms` of the commit
    /// applied there. Empty at seq 0, and `None` below the log's floor.
    pub fn token_at(&self, seq: u64) -> Option<(String, i64)> {
        if seq == 0 {
            return Some((String::new(), 0));
        }
        if seq == self.log_floor_seq {
            return self.floor_token.clone();
        }
        (self.log.iter())
            .find(|record| record.seq == seq)
            .map(|record| (record.commit_id.clone(), record.committed_at_ms))
    }

    /// Whether a client's token at `seq` names the commit this server holds there. An empty token
    /// claims nothing, and neither does one below the floor, whose commit the log no longer holds.
    fn holds_token(&self, seq: u64, commit_id: &str, committed_at_ms: i64) -> bool {
        commit_id.is_empty()
            || seq == 0
            || seq < self.log_floor_seq
            || self.token_at(seq) == Some((commit_id.to_owned(), committed_at_ms))
    }

    /// Also the root node's ID.
    pub fn space_id(&self) -> &str {
        &self.space_id
    }

    pub fn history_id(&self) -> &str {
        &self.history_id
    }

    pub fn head_seq(&self) -> u64 {
        self.head_seq
    }

    pub fn log_floor_seq(&self) -> u64 {
        self.log_floor_seq
    }

    /// Every node, live and trashed, as a post-image.
    pub fn nodes(&self) -> Vec<NodeState> {
        self.state
            .nodes
            .values()
            .map(|n| self.state.node_state(n))
            .collect()
    }

    pub(crate) fn recorded_result(&self, commit_id: &str) -> Option<&CommitResult> {
        self.results.get(commit_id).map(|stored| &stored.result)
    }

    pub fn node(&self, node_id: &str) -> Option<NodeState> {
        self.state
            .nodes
            .get(node_id)
            .map(|n| self.state.node_state(n))
    }

    pub fn welcome(&self, session: &Session) -> Welcome {
        Welcome {
            proto: 0,
            server_version: "koidrive-sim".into(),
            format_version: self.format_version,
            history_id: self.history_id.clone(),
            head_seq: self.head_seq,
            log_floor_seq: self.log_floor_seq,
            read_only: session.role == Role::Viewer || session.max_format < self.format_version,
            device_seq: self.device_seq(&session.device_id),
            ..Default::default()
        }
    }

    /// The device's highest accepted `device_seq` in this history, 0 if it has none.
    pub fn device_seq(&self, device_id: &str) -> u64 {
        self.device_seqs.get(device_id).copied().unwrap_or(0)
    }

    /// The backlog after `since_seq` (spec v0 section 7.3), or why the client must bootstrap instead.
    pub fn resume(&self, resume: &Resume) -> Result<Vec<CommitRecord>, ResyncRequired> {
        let reason = if resume.history_id != self.history_id {
            Some(resync_required::Reason::HistoryChanged)
        } else if resume.since_seq > self.head_seq {
            Some(resync_required::Reason::AheadOfServer)
        } else if resume.since_seq < self.log_floor_seq {
            Some(resync_required::Reason::LogTruncated)
        } else if !self.holds_token(
            resume.since_seq,
            &resume.since_commit_id,
            resume.since_committed_at_ms,
        ) {
            Some(resync_required::Reason::AheadOfServer)
        } else {
            None
        };
        if let Some(reason) = reason {
            let restore_seq = match reason {
                resync_required::Reason::HistoryChanged => self.restore_seq,
                _ => 0,
            };
            return Err(ResyncRequired {
                reason: reason as i32,
                restore_seq,
                ..Default::default()
            });
        }
        Ok(self
            .log
            .iter()
            .filter(|r| r.seq > resume.since_seq)
            .cloned()
            .collect())
    }

    /// Takes the storage of `backup`, a copy of this space from earlier in the same history, as a
    /// restore of the Durable Object's storage does: the commits since are lost and their seqs and
    /// device slots are given again. IDs are random on the server, so the IDs minted since are not
    /// minted again, and the chunk objects stay in the bucket.
    pub fn restore(&mut self, backup: &SpaceDo) {
        let ids = self.state.ids.clone();
        let objects = std::mem::take(&mut self.objects);
        *self = backup.clone();
        self.state.ids = ids;
        self.objects = objects;
    }

    pub fn rewind(&mut self, backup: &SpaceDo, now_ms: i64) {
        self.restore(backup);
        self.history_id = self.state.ids.uuid_v7(now_ms as u64);
        self.restore_seq = self.head_seq;
    }

    /// Forgets the log up to and including `floor_seq`, so that older resumes need a bootstrap,
    /// and the results of the commits it held, except each device's latest (RFC 0002 section 2).
    pub fn truncate_log(&mut self, floor_seq: u64) {
        let floor = floor_seq.min(self.head_seq).max(self.log_floor_seq);
        if floor != self.log_floor_seq {
            self.floor_token = self.token_at(floor);
            self.log_floor_seq = floor;
        }
        self.log.retain(|r| r.seq > floor);
        let latest = &self.device_seqs;
        self.results.retain(|_, stored| {
            stored.result.seq > floor
                || latest.get(&stored.device_id) == Some(&stored.request.device_seq)
        });
    }

    /// Answers one RPC the way the server's `call` does. A new commit also returns its record.
    pub fn rpc(
        &mut self,
        session: &Session,
        now_ms: i64,
        request: &RpcRequest,
    ) -> (RpcResponse, Option<CommitRecord>) {
        use rpc_request::Call;
        use rpc_response::Result as R;
        let (result, record) = match &request.call {
            Some(Call::Commit(c)) => match self.commit(session, now_ms, c) {
                Ok(done) => (Ok(R::Commit(done.result)), done.record),
                Err(e) => (Err(e), None),
            },
            Some(Call::PrepareUpload(c)) => (
                self.prepare_upload(session, now_ms, c)
                    .map(R::PrepareUpload),
                None,
            ),
            Some(Call::GetDownloadTargets(c)) => (
                self.download_targets(now_ms, c).map(R::DownloadTargets),
                None,
            ),
            Some(Call::GetVersionChunks(c)) => (self.version_chunks(c).map(R::VersionChunks), None),
            Some(Call::ListVersions(c)) => (self.list_versions(c).map(R::Versions), None),
            Some(Call::ListTree(c)) => (Ok(R::Tree(self.list_tree(c))), None),
            Some(Call::ListTrash(c)) => (Ok(R::Trash(self.list_trash(c))), None),
            None => (
                Err(fail(error::Code::InvalidArgument, "empty request")),
                None,
            ),
        };
        let result = Some(result.unwrap_or_else(R::Error));
        (
            RpcResponse {
                req_id: request.req_id,
                result,
            },
            record,
        )
    }

    fn check_can_write(&self, session: &Session) -> Result<(), Error> {
        if session.role == Role::Viewer {
            return Err(fail(
                error::Code::Forbidden,
                "viewers cannot change this space",
            ));
        }
        if session.max_format < self.format_version {
            return Err(fail(
                error::Code::ReadOnly,
                "this client is older than the space format",
            ));
        }
        Ok(())
    }

    pub fn commit(
        &mut self,
        session: &Session,
        now_ms: i64,
        commit: &Commit,
    ) -> Result<Committed, Error> {
        if commit.history_id != self.history_id {
            return Err(fail(
                error::Code::HistoryMismatch,
                "history_id is not the space's current history",
            ));
        }
        if let Some(stored) = self.results.get(&commit.commit_id) {
            if stored.device_id != session.device_id {
                return Err(fail(
                    error::Code::InvalidArgument,
                    "commit_id belongs to a commit of another device",
                ));
            }
            if stored.request != covered(commit) {
                return Err(fail(
                    error::Code::InvalidArgument,
                    "commit_id belongs to a different commit",
                ));
            }
            return Ok(Committed {
                result: stored.result.clone(),
                record: None,
            });
        }
        let next = self.device_seq(&session.device_id) + 1;
        match commit.device_seq {
            0 => {
                return Err(fail(error::Code::InvalidArgument, "device_seq starts at 1"));
            }
            slot if slot < next => {
                return Err(fail(
                    error::Code::StaleDeviceState,
                    "device_seq is at or below the device's highest accepted one",
                ));
            }
            slot if slot > next => {
                return Err(fail(
                    error::Code::InvalidArgument,
                    format!("device_seq must be {next}, the device's next slot"),
                ));
            }
            _ => {}
        }
        if commit.base_seq > self.head_seq {
            return Err(fail(
                error::Code::AheadOfServer,
                "base_seq is ahead of the server",
            ));
        }
        if !self.holds_token(
            commit.base_seq,
            &commit.base_commit_id,
            commit.base_committed_at_ms,
        ) {
            return Err(fail(
                error::Code::AheadOfServer,
                "the server holds another commit at base_seq",
            ));
        }
        if commit.require_head && commit.base_seq < self.head_seq {
            return Err(fail(
                error::Code::HeadMoved,
                "the space has changed since base_seq",
            ));
        }
        self.check_can_write(session)?;
        if !is_uuid_v7(&commit.commit_id) {
            return Err(fail(
                error::Code::InvalidArgument,
                "commit_id must be a UUIDv7",
            ));
        }
        if commit.ops.is_empty() {
            return Err(fail(
                error::Code::InvalidArgument,
                "a commit needs at least one op",
            ));
        }
        if commit.ops.len() > MAX_OPS {
            return Err(fail(
                error::Code::LimitExceeded,
                format!("at most {MAX_OPS} ops per commit"),
            ));
        }

        let mut referenced: Vec<&ChunkRef> = Vec::new();
        let mut sizes: BTreeMap<&[u8], u32> = BTreeMap::new();
        for (index, op) in commit.ops.iter().enumerate() {
            check_ids(op, index)?;
            let Some(content) = op_content(op) else {
                continue;
            };
            for chunk in &check_content(content)?.chunks {
                match sizes.insert(chunk.hash.as_slice(), chunk.size) {
                    None => referenced.push(chunk),
                    Some(size) if size != chunk.size => {
                        return Err(fail(
                            error::Code::InvalidArgument,
                            "one chunk hash is listed with two sizes",
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
        let receipted: BTreeSet<&[u8]> = commit
            .receipts
            .iter()
            .filter(|r| {
                sizes.get(r.hash.as_slice()) == Some(&r.size)
                    && self.receipts.get(&r.token).is_some_and(|issued| {
                        issued.hash == r.hash
                            && issued.size == r.size
                            && issued.expires_at_ms >= now_ms
                    })
            })
            .map(|r| r.hash.as_slice())
            .collect();

        let mut missing: Vec<Vec<u8>> = referenced
            .iter()
            .filter(|chunk| {
                let ready = self.state.chunks.get(&chunk.hash).is_some_and(|row| {
                    row.size == chunk.size
                        && (row.state == ChunkState::Live
                            || (row.state == ChunkState::Pending
                                && (receipted.contains(chunk.hash.as_slice())
                                    || self.objects.get(&chunk.hash) == Some(&row.size))))
                });
                !ready
            })
            .map(|chunk| chunk.hash.clone())
            .collect();
        if !missing.is_empty() {
            let total = missing.len();
            missing.truncate(MAX_CHUNKS_PER_CALL);
            let message = format!(
                "{total} chunks are not uploaded; {} are listed",
                missing.len()
            );
            return Err(Error {
                missing_chunks: missing,
                ..fail(error::Code::MissingChunks, message)
            });
        }

        let seq = self.head_seq + 1;
        let mut state = self.state.clone();
        let ctx = Context {
            seq,
            commit_id: &commit.commit_id,
            base_seq: commit.base_seq,
            now_ms,
            committer: session,
        };
        let (results, changes) = Applier::new(&mut state, ctx).apply(&commit.ops)?;
        let record = CommitRecord {
            seq,
            commit_id: commit.commit_id.clone(),
            device_id: session.device_id.clone(),
            user_id: session.user_id.clone(),
            committed_at_ms: now_ms,
            changes: changes.clone(),
            run_id: String::new(),
        };
        let result = CommitResult {
            seq,
            results,
            changes,
            committed_at_ms: now_ms,
        };
        let pushed = ServerMessage {
            msg: Some(server_message::Msg::Changes(Changes {
                commits: vec![record.clone()],
            })),
        };
        let answered = ServerMessage {
            msg: Some(server_message::Msg::Rpc(RpcResponse {
                req_id: u64::MAX,
                result: Some(rpc_response::Result::Commit(result.clone())),
            })),
        };
        if pushed.encoded_len().max(answered.encoded_len()) > MAX_SERVER_MESSAGE_BYTES {
            return Err(fail(
                error::Code::LimitExceeded,
                "the commit's change records do not fit in one 1 MiB message; split the commit",
            ));
        }
        self.state = state;
        self.log.push(record.clone());
        self.results.insert(
            commit.commit_id.clone(),
            StoredResult {
                device_id: session.device_id.clone(),
                request: covered(commit),
                result: result.clone(),
            },
        );
        self.device_seqs
            .insert(session.device_id.clone(), commit.device_seq);
        self.head_seq = seq;
        Ok(Committed {
            result,
            record: Some(record),
        })
    }

    pub fn prepare_upload(
        &mut self,
        session: &Session,
        now_ms: i64,
        request: &PrepareUpload,
    ) -> Result<PrepareUploadResult, Error> {
        self.check_can_write(session)?;
        if request.chunks.len() > MAX_CHUNKS_PER_CALL {
            return Err(fail(
                error::Code::LimitExceeded,
                format!("at most {MAX_CHUNKS_PER_CALL} chunks per call"),
            ));
        }
        request.chunks.iter().try_for_each(check_chunk_ref)?;
        let mut sizes: BTreeMap<&[u8], u32> = BTreeMap::new();
        let mut chunks: Vec<&ChunkRef> = Vec::new();
        for chunk in &request.chunks {
            match sizes.insert(chunk.hash.as_slice(), chunk.size) {
                None => chunks.push(chunk),
                Some(size) if size != chunk.size => {
                    return Err(fail(
                        error::Code::InvalidArgument,
                        "one chunk hash is listed with two sizes",
                    ));
                }
                Some(_) => {}
            }
        }
        let mut chunk_rows = self.state.chunks.clone();
        let mut plans = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let action = match chunk_rows.get_mut(&chunk.hash) {
                None => {
                    chunk_rows.insert(
                        chunk.hash.clone(),
                        ChunkRow {
                            size: chunk.size,
                            state: ChunkState::Pending,
                        },
                    );
                    chunk_plan::Action::Upload
                }
                Some(row) if row.size != chunk.size => {
                    return Err(fail(
                        error::Code::InvalidArgument,
                        "chunk size differs from the stored chunk",
                    ));
                }
                Some(row) => {
                    if row.state == ChunkState::Live {
                        chunk_plan::Action::Present
                    } else {
                        chunk_plan::Action::Upload
                    }
                }
            };
            let target = (action == chunk_plan::Action::Upload).then(|| Target {
                url: format!("sim://chunks/{}", hex(&chunk.hash)),
                method: "PUT".into(),
                expires_at_ms: now_ms + GRANT_MS,
                ..Default::default()
            });
            plans.push(ChunkPlan {
                hash: chunk.hash.clone(),
                action: action as i32,
                target,
                retry_after_ms: 0,
            });
        }
        self.state.chunks = chunk_rows;
        Ok(PrepareUploadResult { chunks: plans })
    }

    /// The relay `PUT` of one chunk at `now_ms`: stores it in the bucket and returns the receipt a
    /// commit presents, valid for this space, chunk and size until `now_ms` plus one hour.
    pub fn put_chunk(&mut self, hash: &[u8], size: u32, now_ms: i64) -> UploadReceipt {
        self.objects.insert(hash.to_vec(), size);
        let token = format!("receipt:{}:{}", self.space_id, self.receipts.len() + 1);
        self.receipts.insert(
            token.clone(),
            IssuedReceipt {
                hash: hash.to_vec(),
                size,
                expires_at_ms: now_ms + GRANT_MS,
            },
        );
        UploadReceipt {
            hash: hash.to_vec(),
            size,
            token,
        }
    }

    pub fn download_targets(
        &self,
        now_ms: i64,
        request: &GetDownloadTargets,
    ) -> Result<DownloadTargets, Error> {
        if request.hashes.len() > MAX_CHUNKS_PER_CALL {
            return Err(fail(
                error::Code::LimitExceeded,
                format!("at most {MAX_CHUNKS_PER_CALL} chunks per call"),
            ));
        }
        let missing: Vec<Vec<u8>> = request
            .hashes
            .iter()
            .filter(|h| {
                self.state
                    .chunks
                    .get(*h)
                    .is_none_or(|row| row.state != ChunkState::Live)
            })
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(Error {
                missing_chunks: missing,
                ..fail(error::Code::MissingChunks, "some chunks are not stored")
            });
        }
        let targets = request
            .hashes
            .iter()
            .map(|hash| ChunkTarget {
                hash: hash.clone(),
                target: Some(Target {
                    url: format!("sim://chunks/{}", hex(hash)),
                    method: "GET".into(),
                    expires_at_ms: now_ms + GRANT_MS,
                    ..Default::default()
                }),
            })
            .collect();
        Ok(DownloadTargets { targets })
    }

    pub fn version_chunks(&self, request: &GetVersionChunks) -> Result<VersionChunks, Error> {
        let version = self
            .state
            .versions
            .get(&request.version_id)
            .ok_or_else(|| fail(error::Code::InvalidArgument, "unknown version"))?;
        let page_count = version.content.chunks.len().div_ceil(PAGE_CHUNKS).max(1) as u32;
        if request.page >= page_count {
            return Err(fail(error::Code::InvalidArgument, "page out of range"));
        }
        let chunks = version
            .content
            .chunks
            .chunks(PAGE_CHUNKS)
            .nth(request.page as usize)
            .unwrap_or_default()
            .to_vec();
        Ok(VersionChunks {
            chunks,
            page: request.page,
            page_count,
        })
    }

    /// The retained versions of a node, newest first by `(seq, version_id)`, with the server's
    /// `<seq>/<version_id>` cursor, so versions written by one commit are never split or skipped.
    pub fn list_versions(&self, request: &ListVersions) -> Result<VersionList, Error> {
        let limit = if request.limit == 0 {
            100
        } else {
            request.limit.min(1000)
        } as usize;
        let before: (i64, &str) = if request.cursor.is_empty() {
            (MAX_SAFE_INTEGER, "")
        } else {
            let mut parts = request.cursor.split('/');
            let seq = js_number(parts.next().unwrap_or_default())
                .filter(|n| n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER as f64);
            let Some(seq) = seq else {
                return Err(fail(error::Code::InvalidArgument, "malformed cursor"));
            };
            (seq as i64, parts.next().unwrap_or_default())
        };
        let mut found: Vec<&Version> = self
            .state
            .versions
            .values()
            .filter(|v| {
                v.node_id == request.node_id && (v.seq as i64, v.version_id.as_str()) < before
            })
            .collect();
        found.sort_by(|a, b| (b.seq, &b.version_id).cmp(&(a.seq, &a.version_id)));
        let page: Vec<&Version> = found.iter().take(limit).copied().collect();
        let next_cursor = match page.last() {
            Some(last) if found.len() > limit => format!("{}/{}", last.seq, last.version_id),
            _ => String::new(),
        };
        Ok(VersionList {
            versions: page
                .into_iter()
                .map(|v| self.state.version_info(v))
                .collect(),
            next_cursor,
        })
    }

    fn node_page(
        &self,
        status: NodeStatus,
        cursor: &str,
        requested: u32,
    ) -> (Vec<NodeState>, String) {
        let limit = if requested == 0 {
            1000
        } else {
            requested.min(5000)
        } as usize;
        let found: Vec<&Node> = self
            .state
            .nodes
            .values()
            .filter(|n| n.status == status && n.node_id.as_str() > cursor)
            .take(limit + 1)
            .collect();
        let mut nodes = Vec::new();
        let mut bytes = 0;
        for node in found.iter().take(limit) {
            let state = self.state.node_state(node);
            bytes += state.encoded_len();
            if !nodes.is_empty() && bytes > PAGE_BYTES {
                break;
            }
            nodes.push(state);
        }
        let next_cursor = if nodes.len() < found.len() {
            nodes.last().map(|n| n.node_id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        (nodes, next_cursor)
    }

    /// One page of the live tree in `node_id` order (spec v0 section 7.5).
    pub fn list_tree(&self, request: &ListTree) -> TreePage {
        let (nodes, next_cursor) = self.node_page(NodeStatus::Live, &request.cursor, request.limit);
        let (head_commit_id, head_committed_at_ms) =
            self.token_at(self.head_seq).unwrap_or_default();
        TreePage {
            nodes,
            next_cursor,
            head_seq: self.head_seq,
            head_commit_id,
            head_committed_at_ms,
        }
    }

    pub fn list_trash(&self, request: &ListTrash) -> TrashPage {
        let (nodes, next_cursor) =
            self.node_page(NodeStatus::Trashed, &request.cursor, request.limit);
        TrashPage {
            nodes,
            next_cursor,
            ..TrashPage::default()
        }
    }
}

/// Every record a commit can carry is a `NodeState` in this model.
pub fn node_states(changes: &[ChangeRecord]) -> impl Iterator<Item = &NodeState> {
    changes.iter().filter_map(|c| match &c.change {
        Some(koidrive_proto::v1::change_record::Change::Node(n)) => Some(n),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use koidrive_engine::names::name_key;
    use koidrive_proto::v1::{CreateDir, Delete, op_result};

    use super::*;

    /// A name stored before the `.koidrive` reservation is renamed when R8 brings it back to the root.
    #[test]
    fn r8_revival_renames_a_stored_reserved_root_name() {
        let mut space = SpaceDo::new(1, 0);
        let session = Session {
            device_id: "dev-a".into(),
            user_id: "user".into(),
            device_name: "Laptop A".into(),
            role: Role::Owner,
            max_format: 1,
        };
        let mut ids = IdGen::new(2);
        let root = space.space_id().to_string();
        let (dir, child) = (ids.uuid_v7(1), ids.uuid_v7(1));
        let mut commit = |space: &mut SpaceDo, op: op::Op| {
            let commit = Commit {
                commit_id: ids.uuid_v7(1),
                base_seq: space.head_seq(),
                ops: vec![Op { op: Some(op) }],
                history_id: space.history_id().into(),
                device_seq: space.device_seq(&session.device_id) + 1,
                ..Default::default()
            };
            space.commit(&session, 0, &commit).unwrap().result
        };
        commit(
            &mut space,
            op::Op::CreateDir(CreateDir {
                node_id: dir.clone(),
                parent_id: root,
                name: "legacy".into(),
                pending_for: String::new(),
            }),
        );
        commit(
            &mut space,
            op::Op::Delete(Delete {
                node_id: dir.clone(),
                requires: None,
            }),
        );
        let stored = space.state.nodes.get_mut(&dir).unwrap();
        stored.name = ".KoiDrive".into();
        stored.name_key = name_key(".KoiDrive");

        let result = commit(
            &mut space,
            op::Op::CreateDir(CreateDir {
                node_id: child,
                parent_id: dir.clone(),
                name: "child".into(),
                pending_for: String::new(),
            }),
        );
        assert_eq!(result.results[0].code(), op_result::Code::Applied);
        let name = space.node(&dir).unwrap().name;
        assert!(
            name.starts_with(".KoiDrive (conflict from Laptop A "),
            "{name}"
        );
    }
}
