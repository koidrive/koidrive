// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::classify::{Pending, Trees};

use koidrive_engine::commit::{Commits, Failure, Halt, MAX_CONFLICT_ATTEMPTS, MAX_OPS, Reconnect};

use koidrive_engine::effects::{Base, BatchLimit, Effect, EffectRecord, Effects, Location};

use koidrive_engine::first_link::PriorSync;

use koidrive_engine::ids::IdGen;

use koidrive_engine::ignore::{LocalRules, RuleFile, Source};

use koidrive_engine::names::{conflict_name, name_key};

use koidrive_engine::question::{Answer, Question, QuestionKind};

use koidrive_engine::restore::{Outcome, Rejection, Resolved, RestoreIntent};

use koidrive_engine::state::{RenamedOver, StateError};

use koidrive_engine::tree::{
    ContentKey, FileIdentity, Kind, LocalNode, Names, NodeId, RemoteNode, Shape, same,
};

use koidrive_proto::v1::{
    ChangeRecord, ChunkRef, Commit, Content, CreateDir, CreateFile, Delete, Error, ListVersions,
    Move, NodeState, Op, OpResult, PrepareUpload, Restore, Resume, UpdateContent, UploadReceipt,
    VersionInfo, change_record, chunk_plan, error, op, op_result,
};

use koidrive_sim::space::{Role, Session, SpaceDo};
use koidrive_sim::world::{key, split};

use proptest::prelude::*;

use sha2::{Digest, Sha256};

const NOW: i64 = 1_794_268_740_000;

const NAMES: &[&str] = &["a", "A", "b", "c.txt", "C.TXT", "ß", "ss"];

const STEPS: usize = 400;

/// Three bytes that say which device wrote them, so writes of different devices never compare equal.
fn content(device: u8, text: u8) -> Content {
    let bytes = [device, text, text];
    let chunks = vec![ChunkRef {
        hash: Sha256::digest(bytes).to_vec(),
        size: 3,
        encoding: 0,
    }];
    let list: Vec<u8> = chunks
        .iter()
        .flat_map(|c| c.hash.iter().copied().chain(c.size.to_le_bytes()))
        .collect();
    Content {
        size: 3,
        chunk_count: 1,
        manifest_digest: Sha256::digest(list).to_vec(),
        chunks,
        ..Default::default()
    }
}

fn session(device: &str) -> Session {
    Session {
        device_id: device.into(),
        user_id: "user".into(),
        device_name: device.into(),
        role: Role::Owner,
        max_format: 1,
    }
}

#[derive(Clone, Debug)]
struct Action {
    kind: u8,
    target: usize,
    parent: usize,
    name: usize,
    text: u8,
}

fn action(kinds: u8) -> impl Strategy<Value = Action> {
    (
        0..kinds,
        any::<usize>(),
        any::<usize>(),
        0..NAMES.len(),
        0..4u8,
    )
        .prop_map(|(kind, target, parent, name, text)| Action {
            kind,
            target,
            parent,
            name,
            text,
        })
}

fn upload(space: &mut SpaceDo, session: &Session, ops: &[Op]) -> Vec<UploadReceipt> {
    let chunks: Vec<ChunkRef> = ops
        .iter()
        .filter_map(|op| match &op.op {
            Some(op::Op::CreateFile(create)) => create.content.as_ref(),
            Some(op::Op::UpdateContent(update)) => update.content.as_ref(),
            _ => None,
        })
        .flat_map(|content| content.chunks.clone())
        .collect();
    if chunks.is_empty() {
        return Vec::new();
    }
    let plan = space
        .prepare_upload(session, NOW, &PrepareUpload { chunks })
        .unwrap();
    plan.chunks
        .iter()
        .filter(|chunk| chunk.action == chunk_plan::Action::Upload as i32)
        .map(|chunk| space.put_chunk(&chunk.hash, 3, NOW))
        .collect()
}

/// The versions of a node, newest first, as an app lists them.
fn versions(space: &SpaceDo, node_id: &str) -> Vec<VersionInfo> {
    let request = ListVersions {
        node_id: node_id.to_owned(),
        ..Default::default()
    };
    space.list_versions(&request).unwrap().versions
}

/// The server's nodes with this status (1 live, 2 trashed) and, when given, this kind (1 folder,
/// 2 file), without the root.
fn nodes_with(space: &SpaceDo, status: i32, kind: Option<i32>) -> Vec<String> {
    let nodes = space.nodes().into_iter().filter(|n| {
        n.status == status
            && kind.is_none_or(|kind| n.kind == kind)
            && n.node_id != space.space_id()
    });
    nodes.map(|n| n.node_id).collect()
}

/// One commit of one op by another device that has seen everything, on its live nodes. Kind 5
/// restores a node from the trash and kind 6 an earlier version of a file.
fn remote_action(space: &mut SpaceDo, ids: &mut IdGen, device: u8, action: &Action) {
    let root = space.space_id().to_owned();
    let live: Vec<_> = space
        .nodes()
        .into_iter()
        .filter(|n| n.status == 1 && n.node_id != root)
        .collect();
    let dirs: Vec<String> = [root.clone()]
        .into_iter()
        .chain(
            live.iter()
                .filter(|n| n.kind == 1)
                .map(|n| n.node_id.clone()),
        )
        .collect();
    let files: Vec<String> = live
        .iter()
        .filter(|n| n.kind == 2)
        .map(|n| n.node_id.clone())
        .collect();
    let parent = dirs[action.parent % dirs.len()].clone();
    let name = NAMES[action.name].to_string();
    let target =
        |list: &[String]| (!list.is_empty()).then(|| list[action.target % list.len()].clone());
    let all: Vec<String> = live.iter().map(|n| n.node_id.clone()).collect();
    let body = match action.kind {
        0 => op::Op::CreateDir(CreateDir {
            node_id: ids.uuid_v7(NOW as u64),
            parent_id: parent,
            name,
            pending_for: String::new(),
        }),
        1 => op::Op::CreateFile(CreateFile {
            made_over: Vec::new(),
            source_version_id: String::new(),
            node_id: ids.uuid_v7(NOW as u64),
            parent_id: parent,
            name,
            content: Some(content(device, action.text)),
            ..Default::default()
        }),
        2 => match target(&files) {
            Some(node_id) => op::Op::UpdateContent(UpdateContent {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id,
                content: Some(content(device, action.text)),
                resend: false,
            }),
            None => return,
        },
        3 => match target(&all) {
            Some(node_id) => op::Op::Move(Move {
                node_id,
                new_parent_id: parent,
                new_name: name,
                from: None,
                pending_for: String::new(),
            }),
            None => return,
        },
        5 => match target(&nodes_with(space, 2, None)) {
            Some(node_id) => op::Op::Restore(Restore {
                node_id,
                ..Default::default()
            }),
            None => return,
        },
        7 | 9 => {
            let trashed = nodes_with(space, 2, (action.kind == 7).then_some(2));
            match target(&[trashed.as_slice(), files.as_slice()].concat()) {
                Some(node_id) if trashed.contains(&node_id) => {
                    space.purge(&node_id, NOW);
                    return;
                }
                Some(node_id) => op::Op::Delete(Delete {
                    node_id,
                    requires: None,
                    ..Default::default()
                }),
                None => return,
            }
        }
        6 => {
            let earlier = target(&files).map(|node_id| (versions(space, &node_id), node_id));
            match earlier {
                Some((versions, node_id)) if versions.len() > 1 => op::Op::Restore(Restore {
                    node_id,
                    version_id: versions[1 + action.name % (versions.len() - 1)]
                        .version_id
                        .clone(),
                    ..Default::default()
                }),
                _ => return,
            }
        }
        _ => match target(&all) {
            Some(node_id) => op::Op::Delete(Delete {
                node_id,
                requires: None,
                ..Default::default()
            }),
            None => return,
        },
    };
    let session = session("other");
    let ops = vec![Op {
        op: Some(body),
        ..Default::default()
    }];
    let receipts = upload(space, &session, &ops);
    let commit = Commit {
        commit_id: ids.uuid_v7(NOW as u64),
        base_seq: space.head_seq(),
        ops,
        receipts,
        history_id: space.history_id().to_owned(),
        device_seq: space.device_seq(&session.device_id) + 1,
        ..Default::default()
    };
    space.commit(&session, NOW, &commit).unwrap();
    if action.kind == 7
        && let Some(op::Op::Delete(delete)) = &commit.ops[0].op
    {
        space.purge(&delete.node_id, NOW);
    }
}

/// A restore the person asks this device's engine for (kinds 5 and 6), chosen from the server's
/// trash or from a file's versions, as an app lists them. With text 1 a node from the trash gets a
/// position, which may be one the server refuses. With text 2 the version is of a file in the
/// trash. With text 3 the node or the version is one the server does not have, as after a purge.
/// False when there was nothing to choose.
fn request_restore(
    space: &SpaceDo,
    commits: &mut Commits,
    ids: &mut IdGen,
    action: &Action,
) -> bool {
    let target =
        |list: &[String]| (!list.is_empty()).then(|| list[action.target % list.len()].clone());
    let unknown = action.text == 3;
    let intent = if action.kind == 5 {
        let node = match unknown {
            true => Some(ids.uuid_v7(NOW as u64)),
            false => target(&nodes_with(space, 2, None)),
        };
        let places: Vec<String> = [space.space_id().to_owned()]
            .into_iter()
            .chain(nodes_with(space, 1, None))
            .collect();
        let position = (action.text == 1).then(|| {
            let parent = places[action.parent % places.len()].clone();
            (parent, NAMES[action.name].to_owned())
        });
        let (parent, name) = position.unzip();
        node.map(|node| {
            RestoreIntent::from_trash(NodeId::from(node), parent.map(NodeId::from), name)
        })
    } else {
        let status = if action.text == 2 { 2 } else { 1 };
        target(&nodes_with(space, status, Some(2))).map(|node| {
            let versions = versions(space, &node);
            let version = match unknown {
                true => ids.uuid_v7(NOW as u64),
                false => versions[action.name % versions.len()].version_id.clone(),
            };
            RestoreIntent::version(NodeId::from(node), version)
        })
    };
    let Some(intent) = intent else {
        return false;
    };
    assert!(commits.restore(ids.uuid_v7(NOW as u64), intent.unwrap()));
    true
}

/// The outcome the engine must report for a restore whose op the server answered with `result`.
fn outcome_of(result: &OpResult) -> Outcome {
    use op_result::Code;
    match Code::try_from(result.code) {
        Ok(Code::Applied) => Outcome::Restored,
        Ok(Code::AppliedRenamed) => Outcome::Renamed {
            name: result.name.clone(),
        },
        Ok(Code::ConflictCopy) => Outcome::Copied {
            node: NodeId::from(&result.node_id),
            name: result.name.clone(),
        },
        _ => Outcome::Rejected(Rejection::from_reason(&result.reject_reason)),
    }
}

/// What each `Restore` op of `commit` brings back if the server applies it to the space as it
/// is, by the op's index: the file with the content of the version the op names, or every file
/// that went to the trash with the op's node, the node included, with the content it holds
/// there. Empty for a restore from the trash of a node that is not in the trash.
fn restorable(space: &SpaceDo, commit: &Commit) -> BTreeMap<usize, Vec<(String, Vec<u8>)>> {
    let nodes = space.nodes();
    let mut found = BTreeMap::new();
    for (index, op) in commit.ops.iter().enumerate() {
        let Some(op::Op::Restore(restore)) = &op.op else {
            continue;
        };
        let files = if restore.version_id.is_empty() {
            let trashed = || nodes.iter().filter(|n| n.status == 2);
            let node = trashed().find(|n| n.node_id == restore.node_id);
            let mut group: Vec<&NodeState> = node.into_iter().collect();
            let mut next = 0;
            while let Some(parent) = group.get(next).copied() {
                let with_it = |n: &&NodeState| {
                    n.parent_id == parent.node_id && n.trash_group == parent.trash_group
                };
                group.extend(trashed().filter(with_it));
                next += 1;
            }
            let content = |n: &NodeState| n.current.clone()?.content;
            let held = |n: &NodeState| Some((n.node_id.clone(), content(n)?.manifest_digest));
            group.into_iter().filter_map(held).collect()
        } else {
            let request = ListVersions {
                node_id: restore.node_id.clone(),
                ..Default::default()
            };
            let versions = space.list_versions(&request).map(|list| list.versions);
            let version = versions.unwrap_or_default().into_iter();
            let version = version.filter(|v| v.version_id == restore.version_id);
            let content = version.filter_map(|v| v.content);
            content
                .map(|c| (restore.node_id.clone(), c.manifest_digest))
                .collect()
        };
        found.insert(index, files);
    }
    found
}

/// What the restores of a commit this device sent did to the writes the property tracks, with
/// the server's content before and after the commit and what each restore was to bring back
/// ([`restorable`], taken before the commit).
///
/// A version restore the server applied replaces the file's content because the person asked for
/// that: the content the server held, and an edit of the file this commit sent ahead of the
/// restore, are versions now and no lost writes.
///
/// The content a restore made current again is a write of the device that wrote it and must be
/// kept from here on: the version's content on the file, or on the copy the server made of it,
/// and the content of each file the restore brought back from the trash. Nothing else that came
/// back in the same commit counts. A move or an edit of a trashed node revives it with its
/// folders (R8), and what that revives is content the device already acknowledged.
///
/// A file a restore brought back from the trash is left out while the device holds an unsent
/// edit of it made on that same content. The person asked for the file, not for its old content
/// over the edit, so the edit replaces it next, as an edit beats a delete. A version restore has
/// no such exception: it asks for that content, and an edit made before it must not replace it.
fn restored(
    commit: &Commit,
    results: &[OpResult],
    bringing: &BTreeMap<usize, Vec<(String, Vec<u8>)>>,
    before: &BTreeSet<(String, Vec<u8>)>,
    after: &BTreeSet<(String, Vec<u8>)>,
    trees: &Trees,
    [local, remote]: [&mut BTreeSet<Vec<u8>>; 2],
) {
    use op_result::Code;
    let applied = |result: &OpResult| {
        result.code == Code::Applied as i32 || result.code == Code::AppliedRenamed as i32
    };
    for (index, (op, result)) in commit.ops.iter().zip(results).enumerate() {
        let version_restore = match &op.op {
            Some(op::Op::Restore(restore)) if !restore.version_id.is_empty() => restore,
            _ => continue,
        };
        if !applied(result) {
            continue;
        }
        let node = &version_restore.node_id;
        let held = before.iter().filter(|(id, _)| id == node);
        let held = held.map(|(_, digest)| digest.clone());
        let sent = commit.ops[..index].iter().zip(results);
        let sent = sent.filter_map(|(op, result)| match &op.op {
            Some(op::Op::UpdateContent(update)) if update.node_id == *node && applied(result) => {
                Some(update.content.as_ref()?.manifest_digest.clone())
            }
            _ => None,
        });
        for digest in held.chain(sent) {
            if !after.contains(&(node.clone(), digest.clone())) {
                local.remove(&digest);
                remote.remove(&digest);
            }
        }
    }
    let mut current = Vec::new();
    for (index, files) in bringing {
        let result = &results[*index];
        let copied = result.code == Code::ConflictCopy as i32;
        if !applied(result) && !copied {
            continue;
        }
        let from_trash =
            matches!(&commit.ops[*index].op, Some(op::Op::Restore(r)) if r.version_id.is_empty());
        for (node, digest) in files {
            let node = NodeId::from(if copied { &result.node_id } else { node });
            let on_disk = (trees.local().get(&node)).and_then(|n| n.shape.content.as_ref());
            let edited = from_trash
                && on_disk.is_some_and(|c| c.manifest_digest() != digest.as_slice())
                && acknowledged(trees, &node).as_ref() == Some(digest);
            if after.contains(&(node.to_string(), digest.clone())) && !edited {
                current.push(digest.clone());
            }
        }
    }
    local.extend(own(current.iter().cloned(), 1));
    remote.extend(own(current.into_iter(), 2));
}

/// Where the process stops and starts again, with the engine state as the store hands it back
/// (the crash table of docs/design/sync-engine.md section 2.8). Without one, the state is still
/// reloaded at the top of every step, which is the stop after a request was taken and the stop
/// after an acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cut {
    None,
    /// After a commit that carries a restore was built and saved, before it left.
    Built,
    /// After the server applied such a commit, before its response arrived.
    Applied,
    /// After the result of such a commit was saved, before R held its records.
    Answered,
    /// While an effect is in flight.
    Effect,
}

/// An identity with inode `ino`. Entries with an odd inode stand for a filesystem that reports no
/// birth time.
fn identity(ino: u64) -> FileIdentity {
    FileIdentity {
        ino: ino.into(),
        birth_ns: ino.is_multiple_of(2).then_some(1_700_000_000_000_000_000),
        handle: None,
    }
}

/// Applies change records to R, as the receiving side does for records in seq order.
fn apply(trees: &mut Trees, changes: &[ChangeRecord]) {
    for change in changes {
        match &change.change {
            Some(change_record::Change::Node(state)) if state.node_id != trees.remote().root => {
                let (id, node) = RemoteNode::from_state(state).unwrap();
                trees.set_remote(id, node);
            }
            Some(change_record::Change::Purged(purged)) => {
                trees.remove_remote(&NodeId::from(&purged.node_id))
            }
            _ => {}
        }
    }
}

/// A change to what this device ignores, made while it runs (section 5.10).
#[derive(Clone, Debug)]
enum Ruling {
    /// The lines of the `.koiignore` file at the root.
    File(Vec<&'static str>),
    /// The device-local exclude list.
    Exclude(Vec<&'static str>),
    /// The device-local include list.
    Include(Vec<&'static str>),
}

const PATTERNS: &[&str] = &["a", "/b", "*.txt", "ss/", "local-*", "!a", "c.txt"];

const INCLUDES: &[&str] = &["a", "b/", "ß", "local-1/"];

fn ruling() -> impl Strategy<Value = Ruling> {
    let lines =
        |pool: &'static [&'static str]| prop::collection::vec(prop::sample::select(pool), 0..3);
    prop_oneof![
        lines(PATTERNS).prop_map(Ruling::File),
        lines(PATTERNS).prop_map(Ruling::Exclude),
        lines(INCLUDES).prop_map(Ruling::Include),
    ]
}

/// The rules as the disk and the configuration hold them. They are not part of the stored
/// state, so the device reads them again after every restart.
#[derive(Clone, Debug, Default)]
struct Rules {
    file: Vec<&'static str>,
    exclude: Vec<&'static str>,
    include: Vec<&'static str>,
}

impl Rules {
    fn read(&self, trees: &mut Trees) {
        let root: [&str; 0] = [];
        let file = RuleFile::parse(self.file.join("\n").as_bytes());
        trees.set_rules(
            &root,
            Source::Koiignore,
            (!self.file.is_empty()).then_some(file),
        );
        trees.set_local_rules(LocalRules::new(&self.include, &self.exclude).unwrap());
        trees.rules_scanned();
    }
}

/// The names from the root to `id` in L.
fn local_path(trees: &Trees, id: &NodeId) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = id;
    while let Some(node) = trees.local().get(current) {
        names.push(node.shape.name.clone());
        current = &node.shape.parent;
    }
    names.reverse();
    names
}

struct Device {
    trees: Trees,
    ids: IdGen,
    inos: u64,
    /// Whether the folders the user creates take names nothing else has, so none is merged.
    fresh_dir_names: bool,
    rules: Rules,
    touched: BTreeSet<NodeId>,
    acted: BTreeSet<NodeId>,
    acked: BTreeSet<NodeId>,
    removed_from: BTreeMap<NodeId, NodeId>,
    on_server_when_taken_along: BTreeMap<NodeId, BTreeMap<NodeId, NodeId>>,
    kept_back: BTreeSet<NodeId>,
    relocated: BTreeMap<NodeId, Relocation>,
    left_out_folders: BTreeSet<String>,
    ignored: IgnoredDisk,
    anchors: BTreeMap<NodeId, FileIdentity>,
    off_disk: BTreeSet<NodeId>,
}

type DiskPath = (NodeId, Vec<String>);
type IgnoredDisk = BTreeMap<DiskPath, DiskEntry>;

struct DiskEntry {
    kind: Kind,
    content: Option<ContentKey>,
    chunks: Box<[ChunkRef]>,
    original: Option<(NodeId, FileIdentity)>,
}

struct Relocation {
    from: koidrive_engine::tree::Shape,
    to: koidrive_engine::tree::Shape,
}

impl Device {
    fn note_taken_along<'a>(&mut self, folders: impl IntoIterator<Item = &'a NodeId>) {
        let on_server: BTreeMap<NodeId, NodeId> = (self.trees.remote().iter())
            .filter(|(_, remote)| !remote.trashed)
            .map(|(id, remote)| (id.clone(), remote.shape.parent.clone()))
            .collect();
        for folder in folders {
            (self.on_server_when_taken_along).insert(folder.clone(), on_server.clone());
        }
    }

    fn new(trees: Trees) -> Self {
        Self {
            trees,
            ids: IdGen::new(7),
            inos: 0,
            fresh_dir_names: false,
            rules: Rules::default(),
            touched: BTreeSet::new(),
            acted: BTreeSet::new(),
            acked: BTreeSet::new(),
            removed_from: BTreeMap::new(),
            on_server_when_taken_along: BTreeMap::new(),
            kept_back: BTreeSet::new(),
            relocated: BTreeMap::new(),
            left_out_folders: BTreeSet::new(),
            ignored: IgnoredDisk::new(),
            anchors: BTreeMap::new(),
            off_disk: BTreeSet::new(),
        }
    }

    fn relocate(
        &mut self,
        id: &NodeId,
        before: &koidrive_engine::tree::Shape,
        after: &koidrive_engine::tree::Shape,
    ) {
        let from = (self.relocated.remove(id))
            .filter(|moved| moved.to.same_position(before))
            .map_or_else(|| before.clone(), |moved| moved.from);
        if !from.same_position(after) {
            let to = after.clone();
            self.relocated
                .insert(id.to_owned(), Relocation { from, to });
        }
    }

    fn receive(&mut self, changes: &[ChangeRecord], answered: bool) {
        for change in changes {
            apply(&mut self.trees, std::slice::from_ref(change));
            if let Some(change_record::Change::Purged(purged)) = &change.change {
                self.acted.remove(&NodeId::from(&purged.node_id));
            }
            let Some(change_record::Change::Node(state)) = &change.change else {
                continue;
            };
            let node = NodeId::from(&state.node_id);
            let Some(remote) = self.trees.remote().live(&node).cloned() else {
                self.removed_from.remove(&node);
                continue;
            };
            let Some(moved) = self.relocated.get_mut(&node) else {
                continue;
            };
            if remote.same_position(&moved.to) {
                self.relocated.remove(&node);
            } else if answered {
                moved.from = remote;
            }
        }
    }

    fn identity(&mut self) -> FileIdentity {
        self.inos += 1;
        identity(self.inos)
    }

    fn rule(&mut self, ruling: &Ruling) {
        match ruling {
            Ruling::File(lines) => self.rules.file = lines.clone(),
            Ruling::Exclude(lines) => self.rules.exclude = lines.clone(),
            Ruling::Include(paths) => self.rules.include = paths.clone(),
        }
        self.rules.read(&mut self.trees);
        let root = self.trees.local().root.clone();
        let let_in: Vec<String> = (self.left_out_folders.iter())
            .filter(|name| !self.trees.ignores(&[name.as_str()], Kind::Dir))
            .cloned()
            .collect();
        for name in let_in {
            self.left_out_folders.remove(&name);
            let node = LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape: koidrive_engine::tree::Shape {
                    parent: root.clone(),
                    name,
                    kind: Kind::Dir,
                    content: None,
                },
                identity: self.identity(),
                parked: false,
                chunks: Box::default(),
            };
            let id = NodeId::from(self.ids.uuid_v7(NOW as u64));
            self.acted_on(id.clone());
            self.trees.set_local_changed(id, node);
        }
        self.rescan();
    }

    fn find_ignored_entries_let_in(&mut self) -> bool {
        let mut found = BTreeMap::new();
        let mut placed_any = false;
        for ((anchor, path), entry) in std::mem::take(&mut self.ignored) {
            let (holder, depth) = (1..path.len())
                .rev()
                .find_map(|depth| {
                    found
                        .get(&(anchor.clone(), path[..depth].to_vec()))
                        .map(|holder| (holder, depth))
                })
                .unwrap_or((&anchor, 0));
            let below = path[depth..].to_vec();
            if below.len() != 1
                || self
                    .disk_path(holder, &below)
                    .is_none_or(|path| self.trees.ignores(&path, entry.kind))
            {
                self.ignored.insert((holder.clone(), below), entry);
                continue;
            }
            let kind = entry.kind;
            let placed = self.place_found(holder, &below[0], entry);
            placed_any = true;
            if kind == Kind::Dir {
                found.insert((anchor, path), placed);
            }
        }
        placed_any
    }

    fn place_found(&mut self, holder: &NodeId, name: &str, entry: DiskEntry) -> NodeId {
        let original = entry.original.filter(|(id, _)| {
            self.trees.local().get(id).is_some() && self.trees.recorded_local(id)
        });
        let (id, identity) = original
            .unwrap_or_else(|| (NodeId::from(self.ids.uuid_v7(NOW as u64)), self.identity()));
        let old = self.trees.local().get(&id);
        let left = old.map(|node| node.shape.parent.clone());
        let node = LocalNode {
            left_out_when_placed: old.is_some_and(|node| node.left_out_when_placed),
            content_from_server: old.is_some_and(|node| node.content_from_server)
                && old.and_then(|node| node.shape.content.as_ref()) == entry.content.as_ref(),
            shape: Shape {
                parent: holder.clone(),
                name: name.to_owned(),
                kind: entry.kind,
                content: entry.content,
            },
            identity,
            parked: false,
            chunks: entry.chunks,
        };
        self.acted_on(id.clone());
        self.off_disk.remove(&id);
        self.trees.set_local_changed(id.clone(), node);
        let mut above = left;
        while let Some(folder) = above.filter(|folder| {
            self.trees.lost_local().contains(folder)
                && self.trees.local_children(folder).next().is_none()
        }) {
            above = (self.trees.local().get(&folder)).map(|node| node.shape.parent.clone());
            self.off_disk.remove(&folder);
            self.removed_from
                .insert(folder.clone(), above.clone().expect("lost folder in L"));
            self.kept_back.remove(&folder);
            self.relocated.remove(&folder);
            self.acted_on(folder.clone());
            self.trees.remove_local(&folder);
        }
        id
    }

    fn disk_path(&self, holder: &NodeId, below: &[String]) -> Option<Vec<String>> {
        if *holder != self.trees.local().root && self.trees.local().get(holder).is_none() {
            return None;
        }
        let mut path = local_path(&self.trees, holder);
        path.extend_from_slice(below);
        Some(path)
    }

    fn on_disk(&self, id: &NodeId) -> bool {
        let local = self.trees.local();
        let mut current = id;
        for _ in 0..=local.len() {
            if self.off_disk.contains(current) {
                return false;
            }
            match local.get(current) {
                Some(node) if !node.parked => current = &node.shape.parent,
                _ => return true,
            }
        }
        false
    }

    fn rescan(&mut self) {
        self.find_ignored_entries_let_in();
        self.list_folders();
    }

    fn list_folders(&mut self) {
        let local = self.trees.local();
        let folders: Vec<NodeId> = std::iter::once(local.root.clone())
            .chain(
                (local.iter())
                    .filter(|(id, node)| node.shape.kind == Kind::Dir && visible(&self.trees, id))
                    .map(|(id, _)| id.clone()),
            )
            .collect();
        self.trees.listed(&folders, Pending::default());
    }

    fn list_moved(&mut self) {
        if self.find_ignored_entries_let_in() {
            self.list_folders();
        }
        let moved: Vec<NodeId> = (self.trees.take_relist().into_iter())
            .filter(|id| visible(&self.trees, id))
            .collect();
        self.trees.listed(&moved, Pending::default());
    }

    fn acted_on(&mut self, id: NodeId) {
        self.acked.remove(&id);
        self.acted.insert(id);
    }

    fn acknowledged(&mut self, commit: &Commit, results: &[OpResult]) {
        for (op, outcome) in commit.ops.iter().zip(results) {
            let id = match &op.op {
                Some(op::Op::Delete(delete)) => &delete.node_id,
                Some(op::Op::CreateFile(create)) => &create.node_id,
                Some(op::Op::CreateDir(create)) => &create.node_id,
                Some(op::Op::Move(moved)) => &moved.node_id,
                Some(op::Op::UpdateContent(update)) => &update.node_id,
                _ => continue,
            };
            use op_result::Code;
            let taken = [
                Code::Applied,
                Code::AppliedRenamed,
                Code::ConflictCopy,
                Code::MergedInto,
            ];
            if taken.iter().any(|code| outcome.code == *code as i32) {
                self.acked.insert(NodeId::from(id));
            }
        }
    }

    /// One change the user makes on disk, as the next rescan reports it. The user sees no parked
    /// node and nothing under one. A scan leaves out ignored paths, so a change at one is not
    /// reported, except a move across the boundary: a node moved to an ignored name keeps its
    /// identity and is reported at that name, one moved into an ignored folder is found there by
    /// its identity, and one moved out is reported where it now is. L keeps the entries of
    /// ignored nodes, so here it still stands for the disk; a node moved into an ignored folder
    /// is out of the user's reach after that, and the user puts nothing where it was: an entry
    /// that takes the path of such a node replaces it, which is a deletion the user made.
    fn act(&mut self, action: &Action) {
        let root = self.trees.local().root.clone();
        let local = self.trees.local();
        let nodes: Vec<(NodeId, LocalNode)> = local
            .iter()
            .filter(|(id, _)| visible(&self.trees, id) && self.on_disk(id))
            .map(|(id, node)| (id.clone(), node.clone()))
            .collect();
        let dirs: Vec<NodeId> = [root.clone()]
            .into_iter()
            .chain(
                nodes
                    .iter()
                    .filter(|(_, n)| n.shape.kind == Kind::Dir)
                    .map(|(id, _)| id.clone()),
            )
            .collect();
        let aimed;
        let action = match action.kind {
            8 | 9 => match aim(&self.trees, &nodes, &dirs, action) {
                Some(found) => {
                    aimed = found;
                    &aimed
                }
                None => return,
            },
            _ => action,
        };
        let outside: Vec<&DiskPath> = if action.kind == 3 {
            (self.ignored.iter())
                .filter(|((holder, below), entry)| {
                    entry.kind == Kind::Dir
                        && entry.original.is_none()
                        && self.disk_path(holder, below).is_some()
                })
                .map(|(path, _)| path)
                .collect()
        } else {
            Vec::new()
        };
        let destination = action.parent % (dirs.len() + outside.len());
        let unique = format!("local-{}", self.inos);
        let name = if action.kind == 0 && self.fresh_dir_names {
            unique.as_str()
        } else {
            NAMES[action.name]
        };
        let target = (!nodes.is_empty()).then(|| nodes[action.target % nodes.len()].clone());
        let unseen = |id: &NodeId, kind| self.trees.ignores(&local_path(&self.trees, id), kind);
        if let Some((holder, below)) = destination.checked_sub(dirs.len()).map(|at| outside[at]) {
            let Some((id, node)) = target else {
                return;
            };
            if self.disk_name_taken(holder, below, name)
                || unseen(&id, node.shape.kind)
                || self.holds(&id, holder)
            {
                return;
            }
            let mut below = below.clone();
            below.push(name.to_owned());
            self.move_to_ignored_disk(&id, holder.clone(), below);
            self.touched.insert(id.clone());
            self.acted_on(id.clone());
            self.trees.ignore_local(&id);
            return;
        }
        let parent = dirs[destination].clone();
        let taken = |id: Option<&NodeId>| {
            local.iter().any(|(other, n)| {
                Some(other) != id
                    && !n.parked
                    && n.shape.parent == parent
                    && name_key(&n.shape.name) == name_key(name)
            }) || self.disk_name_taken(&parent, &[], name)
        };
        let into_ignored = parent != root && unseen(&parent, Kind::Dir);
        let lands_unseen = |kind| {
            let mut path = local_path(&self.trees, &parent);
            path.push(name.to_owned());
            into_ignored || self.trees.ignores(&path, kind)
        };
        match (action.kind, target) {
            (0 | 1, _) if !taken(None) => {
                let (kind, content) = match action.kind {
                    0 => (Kind::Dir, None),
                    _ => (Kind::File, Some(content(1, action.text))),
                };
                let (content, chunks) = split(content);
                if lands_unseen(kind) {
                    if kind == Kind::Dir && lands_unseen(Kind::File) {
                        self.ignored.insert(
                            (parent, vec![name.to_owned()]),
                            DiskEntry {
                                kind,
                                content,
                                chunks,
                                original: None,
                            },
                        );
                    } else if kind == Kind::Dir && parent == root {
                        self.left_out_folders.insert(name.to_owned());
                    }
                    return;
                }
                let shape = koidrive_engine::tree::Shape {
                    parent,
                    name: name.into(),
                    kind,
                    content,
                };
                let node = LocalNode {
                    left_out_when_placed: false,
                    content_from_server: false,
                    shape,
                    identity: self.identity(),
                    parked: false,
                    chunks,
                };
                let id = NodeId::from(self.ids.uuid_v7(NOW as u64));
                self.acted_on(id.clone());
                self.trees.set_local_changed(id, node);
            }
            (2, Some((id, mut node))) if node.shape.kind == Kind::File => {
                if unseen(&id, Kind::File) {
                    return;
                }
                let written = Some(content(1, action.text));
                if node.content() != written {
                    self.touched.insert(id.clone());
                    self.acted_on(id.clone());
                }
                (node.shape.content, node.chunks) = split(written);
                self.trees.set_local_changed(id, node);
            }
            (3, Some((id, mut node))) if !taken(Some(&id)) && !self.holds(&id, &parent) => {
                let kind = node.shape.kind;
                match (unseen(&id, kind), lands_unseen(kind)) {
                    (true, true) => {}
                    _ => {
                        let before = node.shape.clone();
                        node.shape.parent = parent;
                        node.shape.name = name.into();
                        if before.parent != node.shape.parent || before.name != node.shape.name {
                            self.touched.insert(id.clone());
                            self.acted_on(id.clone());
                            self.relocate(&id, &before, &node.shape);
                        }
                        self.trees.set_local_changed(id, node);
                    }
                }
            }
            (4, Some((id, node))) if !unseen(&id, node.shape.kind) => {
                let mut doomed = vec![id];
                let (mut held, mut held_in_l) = (Vec::new(), Vec::new());
                let mut next = 0;
                while let Some(current) = doomed.get(next).cloned() {
                    for child in self.trees.local_children(&current) {
                        if !self.on_disk(child) {
                            held_in_l.push(current.clone());
                        } else if self.trees.ignored_local().contains(child) {
                            held.push(current.clone());
                        } else {
                            doomed.push(child.clone());
                        }
                    }
                    next += 1;
                }
                let anchored = (self.ignored.iter())
                    .filter(|((_, below), entry)| below.len() > 1 || entry.original.is_some())
                    .map(|((holder, _), _)| holder.clone());
                let up = |starts: Vec<NodeId>| {
                    let mut folders = BTreeSet::new();
                    for start in starts {
                        let mut above = Some(start);
                        while let Some(folder) = above.filter(|folder| doomed.contains(folder)) {
                            if !folders.insert(folder.clone()) {
                                break;
                            }
                            above =
                                (self.trees.local().get(&folder)).map(|n| n.shape.parent.clone());
                        }
                    }
                    folders
                };
                let kept = up(held.into_iter().chain(anchored).collect());
                let mut only_in_l = up(held_in_l);
                only_in_l.retain(|folder| !kept.contains(folder));
                doomed.retain(|node| !kept.contains(node) && !only_in_l.contains(node));
                self.ignored.retain(|(holder, _), _| {
                    !doomed.contains(holder) && !only_in_l.contains(holder)
                });
                for folder in &kept {
                    self.acted_on(folder.clone());
                    self.trees.ignore_local(folder);
                }
                for folder in &only_in_l {
                    self.acted_on(folder.clone());
                    self.trees.keep_lost_local(folder);
                }
                self.off_disk.extend(only_in_l);
                for other in doomed.into_iter().rev() {
                    if let Some(node) = self.trees.local().get(&other) {
                        self.removed_from
                            .insert(other.clone(), node.shape.parent.clone());
                    }
                    self.kept_back.remove(&other);
                    self.relocated.remove(&other);
                    self.acted_on(other.clone());
                    self.trees.remove_local(&other);
                }
            }
            _ => {}
        }
    }

    fn left_out_root_holds(&self, name: &str) -> bool {
        let key = name_key(name);
        self.left_out_folders
            .iter()
            .any(|held| name_key(held) == key)
            || self.ignored.iter().any(|((parent, path), entry)| {
                *parent == self.trees.local().root
                    && entry.kind == Kind::Dir
                    && entry.original.is_none()
                    && matches!(path.as_slice(), [held] if name_key(held) == key)
            })
    }

    fn disk_name_taken(&self, holder: &NodeId, below: &[String], name: &str) -> bool {
        let key = name_key(name);
        if *holder == self.trees.local().root
            && below.is_empty()
            && self
                .left_out_folders
                .iter()
                .any(|held| name_key(held) == key)
        {
            return true;
        }
        (self.ignored.keys()).any(|(anchor, path)| {
            anchor == holder
                && path
                    .split_last()
                    .is_some_and(|(rest, parent)| parent == below && name_key(rest) == key)
        })
    }

    fn move_to_ignored_disk(&mut self, id: &NodeId, holder: NodeId, below: Vec<String>) {
        let local = self.trees.local();
        let mut moved = BTreeMap::from([(id.clone(), below)]);
        let mut next = vec![id.clone()];
        while let Some(id) = next.pop() {
            for child in self.trees.local_children(&id) {
                if !self.on_disk(child) || local.get(child).is_some_and(|node| node.parked) {
                    continue;
                }
                let mut below = moved[&id].clone();
                below.push(local.get(child).expect("local child").shape.name.clone());
                moved.insert(child.clone(), below);
                next.push(child.clone());
            }
        }
        self.ignored = (std::mem::take(&mut self.ignored).into_iter())
            .map(|((anchor, path), entry)| match moved.get(&anchor) {
                Some(to) => ((holder.clone(), [to.as_slice(), &path].concat()), entry),
                None => ((anchor, path), entry),
            })
            .collect();
        self.off_disk.extend(moved.keys().cloned());
        for (id, below) in moved {
            let node = local.get(&id).expect("moved node");
            self.ignored.insert(
                (holder.clone(), below),
                DiskEntry {
                    kind: node.shape.kind,
                    content: node.shape.content,
                    chunks: node.chunks.clone(),
                    original: Some((id, node.identity)),
                },
            );
        }
    }

    fn follow_rekeys(&mut self) {
        let local = self.trees.local();
        let rekeyed: BTreeMap<NodeId, NodeId> = (self.anchors.iter())
            .filter(|(id, _)| local.get(id).is_none())
            .filter_map(|(id, identity)| {
                Some((
                    id.clone(),
                    self.trees.local_with_identity(identity)?.clone(),
                ))
            })
            .collect();
        self.ignored = (std::mem::take(&mut self.ignored).into_iter())
            .map(|((anchor, path), entry)| {
                (
                    (rekeyed.get(&anchor).cloned().unwrap_or(anchor), path),
                    entry,
                )
            })
            .collect();
        self.anchors = (self.ignored.keys())
            .filter_map(|(anchor, _)| {
                Some((anchor.clone(), self.trees.local().get(anchor)?.identity))
            })
            .collect();
    }

    /// True when `ancestor` is `id` or holds it in L.
    fn holds(&self, ancestor: &NodeId, id: &NodeId) -> bool {
        let mut current = id;
        loop {
            if current == ancestor {
                return true;
            }
            match self.trees.local().get(current) {
                Some(node) => current = &node.shape.parent,
                None => return false,
            }
        }
    }
}

fn visible(trees: &Trees, id: &NodeId) -> bool {
    let (local, hidden) = (trees.local(), trees.ignored_local());
    let mut current = id;
    for _ in 0..=local.len() {
        if hidden.contains(current) {
            return false;
        }
        match local.get(current) {
            Some(node) if node.parked => return false,
            Some(node) => current = &node.shape.parent,
            None => return true,
        }
    }
    false
}

fn aim(
    trees: &Trees,
    nodes: &[(NodeId, LocalNode)],
    dirs: &[NodeId],
    action: &Action,
) -> Option<Action> {
    let ids: Vec<&NodeId> = nodes.iter().map(|(id, _)| id).collect();
    let at = |id: &NodeId| ids.iter().position(|other| *other == id);
    let dir_at = |id: &NodeId| dirs.iter().position(|other| other == id);
    let pick =
        |list: Vec<NodeId>| (!list.is_empty()).then(|| list[action.target % list.len()].clone());
    let server_dir = |id: &NodeId| (trees.remote().live(id)).is_some_and(|r| r.kind == Kind::Dir);
    let local_only = |id: &NodeId| *id != trees.local().root && trees.remote().get(id).is_none();
    let above = |id: &NodeId| {
        let mut parents = Vec::new();
        let mut current = trees.local().get(id).map(|node| node.shape.parent.clone());
        while let Some(parent) = current.filter(|_| parents.len() <= trees.local().len()) {
            current = trees
                .local()
                .get(&parent)
                .map(|node| node.shape.parent.clone());
            parents.push(parent);
        }
        parents
    };
    if action.kind == 8 {
        let target = pick(dirs.iter().filter(|dir| server_dir(dir)).cloned().collect())?;
        let parents: Vec<&NodeId> = (dirs.iter())
            .filter(|dir| local_only(dir) && **dir != target && !above(dir).contains(&target))
            .collect();
        let parent = (*parents.get(action.parent % parents.len().max(1))?).clone();
        let mut path = local_path(trees, &parent);
        let name = (0..NAMES.len())
            .map(|step| (action.name + step) % NAMES.len())
            .find(|&index| {
                path.push(NAMES[index].to_owned());
                let excluded = trees.ignores(&path, Kind::Dir);
                path.pop();
                excluded
            })
            .unwrap_or(action.name);
        return Some(Action {
            kind: 3,
            target: at(&target)?,
            parent: dir_at(&parent)?,
            name,
            text: action.text,
        });
    }
    let folders: Vec<NodeId> = (dirs.iter())
        .filter(|dir| server_dir(dir) && above(dir).iter().any(local_only))
        .cloned()
        .collect();
    let folder = pick(folders)?;
    let below = |kind: Option<Kind>| -> Vec<NodeId> {
        (nodes.iter())
            .filter(|(_, node)| node.shape.parent == folder)
            .filter(|(_, node)| kind.is_none_or(|kind| node.shape.kind == kind))
            .map(|(id, _)| id.clone())
            .collect()
    };
    let child = |kind: Option<Kind>| pick(below(kind)).and_then(|id| at(&id));
    match action.text % 4 {
        0 => Some(Action {
            kind: (action.name % 2) as u8,
            target: 0,
            parent: dir_at(&folder)?,
            ..action.clone()
        }),
        1 => Some(Action {
            kind: 2,
            target: child(Some(Kind::File))?,
            ..action.clone()
        }),
        2 => Some(Action {
            kind: 4,
            target: child(None)?,
            ..action.clone()
        }),
        _ => Some(Action {
            kind: 3,
            target: child(None)?,
            parent: 0,
            ..action.clone()
        }),
    }
}

fn written(contents: impl Iterator<Item = Option<ContentKey>>, device: u8) -> BTreeSet<Vec<u8>> {
    own(
        contents.flatten().map(|c| c.manifest_digest().to_vec()),
        device,
    )
}

/// The digests among `digests` that `device` wrote.
fn own(digests: impl Iterator<Item = Vec<u8>>, device: u8) -> BTreeSet<Vec<u8>> {
    let mine: BTreeSet<Vec<u8>> = (0..4)
        .map(|text| content(device, text).manifest_digest)
        .collect();
    digests.filter(|digest| mine.contains(digest)).collect()
}

/// The live server nodes that hold content, each with its manifest digest.
fn stored(space: &SpaceDo) -> BTreeSet<(String, Vec<u8>)> {
    space
        .nodes()
        .into_iter()
        .filter(|n| n.status == 1)
        .filter_map(|n| Some((n.node_id, n.current?.content?.manifest_digest)))
        .collect()
}

/// The L nodes that hold content, each with its manifest digest.
/// The content the server acknowledged for a file: its waiting S update's while one waits, and
/// S's otherwise.
fn acknowledged(trees: &Trees, id: &NodeId) -> Option<Vec<u8>> {
    let shape = acknowledged_shape(trees, id)?;
    Some(shape.content.as_ref()?.manifest_digest().to_vec())
}

fn acknowledged_shape<'a>(trees: &'a Trees, id: &NodeId) -> Option<&'a Shape> {
    match trees.waiting().get(id) {
        Some(waiting) => waiting.shape.as_ref(),
        None => trees.synced().live(id),
    }
}

fn held(trees: &Trees) -> BTreeSet<(String, Vec<u8>)> {
    trees
        .local()
        .iter()
        .filter_map(|(id, node)| {
            Some((
                id.to_string(),
                node.shape.content.as_ref()?.manifest_digest().to_vec(),
            ))
        })
        .collect()
}

/// Forgets the writes that left a node holding content in `before` by `after`, which the user
/// or the other device overwrote or deleted, and returns the digests that nodes in `after` hold
/// anew. Content equal to the node's acknowledged content, its waiting S update's while one
/// waits and S's otherwise, is not a write the engine can tell apart from no change at all, so it
/// is left out.
fn rewritten(
    before: &BTreeSet<(String, Vec<u8>)>,
    after: &BTreeSet<(String, Vec<u8>)>,
    trees: &Trees,
    mut writes: [&mut BTreeSet<Vec<u8>>; 2],
) -> Vec<Vec<u8>> {
    for (_, gone) in before.difference(after) {
        for written in writes.iter_mut() {
            written.remove(gone);
        }
    }
    after
        .difference(before)
        .filter(|(id, digest)| acknowledged(trees, &NodeId::from(id)).as_ref() != Some(digest))
        .map(|(_, digest)| digest.clone())
        .collect()
}

/// Rebuilds the engine state the way the store does after a restart, with the rules and the moves
/// into ignored folders that the first scan reports again, and checks that each part comes back
/// equal.
fn reload(
    trees: &Trees,
    commits: &Commits,
    effects: &Effects,
    rules: &Rules,
) -> Result<(Trees, Commits, Effects), TestCaseError> {
    let mut loaded = Trees::new(trees.remote().root.clone());
    for (id, node) in trees.remote().iter() {
        loaded.set_remote(id.clone(), node.clone());
    }
    for (id, node) in trees.local().iter() {
        loaded.set_local(id.clone(), node.clone());
    }
    for (id, shape) in trees.synced().iter() {
        loaded.set_synced(id.clone(), Some(shape.clone()));
        loaded.set_synced_mark(id.clone(), trees.synced_mark(id).map(str::to_owned));
    }
    let refused = |error: StateError| TestCaseError::fail(format!("refused: {error:?}"));
    loaded.restore(trees.state()).map_err(refused)?;
    prop_assert!(!loaded.rules_known());
    rules.read(&mut loaded);
    let restored_commits = Commits::from_state(commits.state()).map_err(refused)?;
    restored_commits.hold(&mut loaded);
    let restored_effects = Effects::from_state(effects.state()).map_err(refused)?;
    prop_assert_eq!(&loaded, trees);
    prop_assert_eq!(&restored_commits, commits);
    prop_assert_eq!(&restored_effects, effects);
    Ok((loaded, restored_commits, restored_effects))
}

/// The server as [`run`] seeds it.
fn seeded(seed: &[Action]) -> (SpaceDo, IdGen) {
    let mut space = SpaceDo::new(5, NOW);
    let mut other = IdGen::new(6);
    for action in seed {
        remote_action(&mut space, &mut other, 0, action);
    }
    (space, other)
}

/// How a run ended.
struct Ran {
    /// The most reverted nodes that were parked at one time.
    parked_reverted: usize,
    /// The restores this device asked for, as they ended.
    resolved: Vec<Resolved>,
    /// The `Delete` ops the device sent.
    deletes: usize,
    /// The effects that removed a file or a folder from the device's disk, not counting a folder
    /// the server never held: one created here and merged into another, removed once empty.
    removals: usize,
    /// The nodes the server holds trashed at the end.
    trashed: usize,
    /// The questions the run answered.
    asked: usize,
    conflicts: usize,
    space: SpaceDo,
}

/// `fits` is the most ops a commit may hold before the server refuses it with `LIMIT_EXCEEDED`,
/// standing in for the 1 MiB message limit (spec v0 section 7.2). Returns the most reverted nodes
/// that were parked at once.
fn run(
    seed: &[Action],
    local: &[Action],
    remote: &[Action],
    fits: usize,
    fresh_dir_names: bool,
    during: &[(bool, Vec<Action>)],
) -> Result<usize, TestCaseError> {
    let ran = run_cut(
        seed,
        local,
        remote,
        fits,
        fresh_dir_names,
        during,
        Cut::None,
    )?;
    Ok(ran.parked_reverted)
}

/// Every node a question holds.
fn members(question: &Question) -> impl Iterator<Item = NodeId> + '_ {
    (question.held.values()).flat_map(|held| held.members.keys().cloned())
}

fn held_by(questions: &[Question]) -> BTreeSet<NodeId> {
    (questions.iter())
        .flat_map(|question| question.shown.keys().cloned().chain(members(question)))
        .collect()
}

/// Whether the effects cannot place `id` because of an excluded node: on the way from the node's
/// R position to the root, a name is held in L by an excluded node, whose local copy stays where
/// it is, or by a node that waits the same way, or a folder on the way waits itself or is
/// excluded and not where R has it. A folder R
/// no longer holds waits when L still holds such a node in it, since only an empty folder is
/// removed. Such a node stays as it is, and the driver reports it as a problem (section 5.1).
fn waits_on_excluded(trees: &Trees, id: &NodeId, waiting: &BTreeSet<NodeId>) -> bool {
    let mut current = id;
    for _ in 0..=trees.remote().len() {
        let Some(remote) = trees.remote().live(current) else {
            let stays = |other: &NodeId| trees.ignored(other) || waiting.contains(other);
            return current == id
                && ((trees.local().iter())
                    .any(|(other, node)| node.shape.parent == *id && stays(other))
                    || (trees.synced().iter())
                        .any(|(other, shape)| shape.parent == *id && waiting.contains(other)));
        };
        let placed = |node: &LocalNode| !node.parked && node.shape.same_position(remote);
        let unplaced = !trees.local().get(current).is_some_and(placed);
        if current != id && (waiting.contains(current) || (unplaced && trees.ignored(current))) {
            return true;
        }
        let key = name_key(&remote.name);
        let holder = trees.local().iter().find(|(other, node)| {
            *other != current
                && !node.parked
                && node.shape.parent == remote.parent
                && name_key(&node.shape.name) == key
        });
        if holder.is_some_and(|(other, _)| trees.excluded(other) || waiting.contains(other)) {
            return true;
        }
        if remote.parent == trees.remote().root {
            return false;
        }
        current = &remote.parent;
    }
    false
}

fn fail_on_ignored_name(
    device: &mut Device,
    effects: &mut Effects,
    record: &EffectRecord,
    failed_on_ignored: &mut BTreeMap<NodeId, (Effect, Shape)>,
) -> bool {
    let placed_at = match &record.effect {
        Effect::CreateDir {
            at: Location::At { .. },
        }
        | Effect::WriteFile {
            at: Location::At { .. },
            ..
        }
        | Effect::Move {
            to: Location::At { .. },
            ..
        } => record.after.as_ref(),
        _ => None,
    };
    let Some(at) = placed_at.filter(|at| device.disk_name_taken(&at.parent, &[], &at.name)) else {
        return false;
    };
    let target = (record.effect.clone(), at.clone());
    if at.parent == device.trees.local().root && device.left_out_root_holds(&at.name) {
        effects.taken(&mut device.trees, &record.effect_id, Some(Kind::Dir))
    } else {
        effects.failed(&mut device.trees, &record.effect_id)
    }
    .unwrap();
    failed_on_ignored.insert(record.node.clone(), target);
    true
}

fn still_planned_target<'a>(
    device: &Device,
    effects: &Effects,
    failed_on_ignored: &'a BTreeMap<NodeId, (Effect, Shape)>,
    in_flight: Option<&koidrive_engine::commit::InFlight>,
    id: &NodeId,
) -> Option<&'a Shape> {
    let (failed, at) = failed_on_ignored.get(id)?;
    if effects.blocked().get(id) != Some(failed) {
        return None;
    }
    let mut trees = device.trees.clone();
    let mut ids = device.ids.clone();
    let mut fresh = Effects::default();
    for _ in 0..=trees.local().len() + trees.remote().len() {
        let record = (fresh.plan(&trees, in_flight, &mut ids, NOW as u64, BatchLimit::ONE))
            .first()?
            .clone();
        if record.node == *id {
            return (record.effect == *failed).then_some(at);
        }
        fresh.failed(&mut trees, &record.effect_id).ok()?;
    }
    None
}

fn waits_on_left_out(device: &Device, id: &NodeId) -> bool {
    let Some(remote) = device.trees.remote().live(id) else {
        return false;
    };
    remote.parent == device.trees.remote().root
        && device.left_out_root_holds(&remote.name)
        && (remote.kind == Kind::Dir
            || (1..=MAX_CONFLICT_ATTEMPTS).all(|attempt| {
                let name = conflict_name(&remote.name, "", NOW, attempt);
                device.trees.ignores(&[name], remote.kind)
            }))
}

/// Whether S cannot take `id` at its R position because the S entry of a node that waits still
/// holds that name. The entry moves only once its node is placed, which the excluded node keeps
/// from happening.
fn waits_on_synced(trees: &Trees, id: &NodeId, waiting: &BTreeSet<NodeId>) -> bool {
    let Some(remote) = trees.remote().live(id) else {
        return false;
    };
    let key = name_key(&remote.name);
    trees.synced().iter().any(|(other, shape)| {
        other != id
            && waiting.contains(other)
            && shape.parent == remote.parent
            && name_key(&shape.name) == key
    })
}

/// [`run`], with the process stopping at `cut`. An action of kind 5 or 6 on this device asks its
/// engine for a restore.
fn run_cut(
    seed: &[Action],
    local: &[Action],
    remote: &[Action],
    fits: usize,
    fresh_dir_names: bool,
    during: &[(bool, Vec<Action>)],
    cut: Cut,
) -> Result<Ran, TestCaseError> {
    run_ignoring(
        seed,
        local,
        remote,
        fits,
        fresh_dir_names,
        during,
        cut,
        &[],
        Answer::Delete,
        None,
    )
}

fn still_discarded_in(
    device: &Device,
    id: &NodeId,
    folder: &NodeId,
    discarded: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
) -> bool {
    let trees = &device.trees;
    (discarded.get(id))
        .zip(trees.local().get(id))
        .is_some_and(|(shown, local)| shown.parent == *folder && local.shape.same(shown))
        && !removed_by_user_from(device, id)
        && (trees.local().iter())
            .filter(|(_, node)| node.shape.parent == *id)
            .all(|(child, _)| still_discarded_in(device, child, id, discarded))
}

fn removed_by_user_from(device: &Device, id: &NodeId) -> bool {
    let trees = &device.trees;
    let remote = trees.remote();
    let held_by_r = |child: &NodeId| {
        let mut seen = BTreeSet::new();
        let mut at = remote.live(child).map(|shape| shape.parent.clone());
        while let Some(parent) = at.filter(|parent| seen.insert(parent.clone())) {
            if parent == *id {
                return true;
            }
            at = remote.get(&parent).map(|node| node.shape.parent.clone());
        }
        false
    };
    (device.removed_from.iter()).any(|(child, from)| {
        trees.remote().live(child).is_some()
            && trees.local().get(child).is_none()
            && from == id
            && held_by_r(child)
            && (acknowledged_shape(trees, child).is_some() || trees.excluded(child))
            && !kept_by_r7(device, child)
    })
}

fn server_copy(
    device: &Device,
    id: &NodeId,
    shown: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
    from_server: &BTreeSet<NodeId>,
    discarded: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
    kept: &mut BTreeMap<NodeId, koidrive_engine::tree::Shape>,
) -> bool {
    let trees = &device.trees;
    let remote = (trees.remote().get(id).map(|remote| &remote.shape))
        .or_else(|| acknowledged_shape(trees, id).filter(|_| !discarded.contains_key(id)));
    let (Some(local), Some(remote)) = (trees.local().get(id), remote) else {
        return false;
    };
    let mut below = BTreeMap::new();
    let copy = match (&local.shape.content, &remote.content) {
        (Some(local), Some(remote)) => {
            koidrive_engine::tree::same_content(local, remote) || from_server.contains(id)
        }
        (None, None) => {
            !removed_by_user_from(device, id)
                && (trees.local_children(id)).all(|child| {
                    shown.contains_key(child)
                        || still_discarded_in(device, child, id, discarded)
                        || server_copy(device, child, shown, from_server, discarded, &mut below)
                })
        }
        _ => false,
    };
    if copy {
        kept.append(&mut below);
        kept.insert(id.to_owned(), local.shape.clone());
    }
    copy
}

fn taken_along(
    device: &Device,
    shown: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
    (from_server, placed_left_out): (&BTreeSet<NodeId>, &BTreeSet<NodeId>),
    discarded: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
) -> BTreeMap<NodeId, koidrive_engine::tree::Shape> {
    let mut taken = BTreeMap::new();
    for folder in shown.keys() {
        for child in device.trees.local_children(folder) {
            if !shown.contains_key(child)
                && (device.trees.excluded(child) || placed_left_out.contains(child))
            {
                server_copy(device, child, shown, from_server, discarded, &mut taken);
            }
        }
    }
    taken
}

fn only_server_copies_below(
    device: &Device,
    folder: &NodeId,
    shown: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
    (from_server, placed_left_out): (&BTreeSet<NodeId>, &BTreeSet<NodeId>),
    discarded: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
) -> bool {
    let sets = (from_server, placed_left_out);
    (device.trees.local_children(folder)).all(|child| match shown.get(child) {
        Some(shape) => {
            shape.kind == Kind::File
                || only_server_copies_below(device, child, shown, sets, discarded)
        }
        None => {
            (device.trees.excluded(child) || placed_left_out.contains(child))
                && server_copy(
                    device,
                    child,
                    shown,
                    from_server,
                    discarded,
                    &mut BTreeMap::new(),
                )
        }
    })
}

fn revive(
    device: &Device,
    discarded: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
    kept_with: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
    asked: &BTreeSet<NodeId>,
) -> BTreeSet<NodeId> {
    // A discarded entry the user changed, or a discarded folder that holds an entry the answer
    // did not discard there, is the user's, and so is every discarded folder above it.
    let trees = &device.trees;
    let reclaimed = |id: &&NodeId| {
        let changed = device.touched.contains(*id)
            && !asked.contains(*id)
            && !trees.excluded(id)
            && (trees.local().get(id)).is_some_and(|local| {
                !discarded
                    .get(*id)
                    .is_some_and(|shown| local.shape.same(shown))
            });
        changed
            || (trees.local_children(id)).any(|child| {
                !trees.excluded(child)
                    && !asked.contains(child)
                    && (!kept_with.contains_key(child)
                        || acted_below(device, child, kept_with, discarded))
                    && discarded
                        .get(child)
                        .is_none_or(|shown| &shown.parent != *id)
            })
    };
    let mut revived = BTreeSet::new();
    let unsent = |id: &&NodeId| trees.remote().get(id).is_none();
    for id in discarded.keys().filter(unsent).filter(reclaimed) {
        let mut next = Some(id.clone());
        while let Some(id) = next.take_if(|id| discarded.contains_key(id)) {
            next = (trees.local().get(&id)).map(|node| node.shape.parent.clone());
            revived.insert(id);
        }
    }
    revived
}

fn release_taken_along(
    trees: &Trees,
    kept_with: &mut BTreeMap<NodeId, Shape>,
    revived: &BTreeSet<NodeId>,
) {
    let reclaimed = |folder: &NodeId| {
        revived.contains(folder)
            && (trees.synced().get(folder).is_none() || trees.remote().get(folder).is_some())
    };
    let home = |id: &NodeId| {
        (trees.local().get(id)).is_some_and(|local| {
            !local.parked
                && (trees.remote().live(id))
                    .is_some_and(|remote| remote.same_position(&local.shape))
        })
    };
    let released: Vec<NodeId> = (kept_with.iter())
        .filter(|(id, shape)| {
            let (mut top, mut folder) = (*id, &shape.parent);
            for _ in 0..=kept_with.len() {
                match kept_with.get_key_value(folder) {
                    Some((kept_id, kept)) => (top, folder) = (kept_id, &kept.parent),
                    None => return reclaimed(folder) || home(top),
                }
            }
            false
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in released {
        kept_with.remove(&id);
    }
}

type Position = (NodeId, String);

fn closes_a_cycle(trees: &Trees, id: &NodeId, parent: &NodeId) -> bool {
    let mut folder = Some(parent.to_owned());
    for _ in 0..=trees.local().len() + trees.remote().len() {
        match folder {
            Some(at) if at == *id => return true,
            Some(at) => {
                folder = (trees
                    .remote()
                    .get(&at)
                    .map(|node| node.shape.parent.clone()))
                .or_else(|| trees.local().get(&at).map(|node| node.shape.parent.clone()))
            }
            None => return false,
        }
    }
    false
}

fn position(shape: &koidrive_engine::tree::Shape) -> Position {
    (shape.parent.clone(), shape.name.clone())
}

fn sent_move_check(
    moved: &Move,
    local: &Shape,
    synced: Option<&Shape>,
    remote: Option<&RemoteNode>,
    device: &str,
    merged_into: &BTreeMap<NodeId, NodeId>,
    refused_at: Option<&RenamedOver>,
) -> Result<(), TestCaseError> {
    let refused_name = refused_at.is_some_and(|place| {
        moved.pending_for.is_empty()
            && moved.new_parent_id == place.parent
            && remote.is_some_and(|node| {
                !node.trashed && node.shape.parent == place.parent && node.shape.name == place.name
            })
            && (moved.from.as_ref())
                .is_some_and(|from| from.parent_id == place.parent && from.name == place.name)
    });
    if refused_name {
        return Ok(());
    }
    let held = position(local);
    let owed_rename = moved.pending_for.is_empty()
        && moved.new_parent_id == local.parent
        && synced.map(position).as_ref() == Some(&held)
        && remote.is_some_and(|node| {
            !node.trashed
                && node.pending_by.as_deref() == Some(device)
                && position(&node.shape) == held
                && node.pending_for.as_ref() == Some(&moved.new_name)
        });
    if owed_rename {
        return Ok(());
    }
    let parent = (merged_into.get(&local.parent)).unwrap_or(&local.parent);
    let finishes_mark = remote.is_some_and(|remote| {
        remote.shape.parent == moved.new_parent_id
            && remote.pending_for.as_ref() == Some(&moved.new_name)
    });
    let from = (moved.from.as_ref()).map(|from| (NodeId::from(&from.parent_id), &from.name));
    if finishes_mark && from == Some((parent.clone(), &local.name)) {
        return Ok(());
    }
    let name = Some(&moved.pending_for)
        .filter(|name| !name.is_empty())
        .unwrap_or(&moved.new_name);
    prop_assert_eq!(
        (&NodeId::from(&moved.new_parent_id), name),
        (parent, &local.name),
        "{} is sent to a place the disk does not hold it at",
        moved.node_id
    );
    Ok(())
}

fn revive_check(
    device: &Device,
    moved: &BTreeSet<NodeId>,
    id: &NodeId,
) -> Result<(), TestCaseError> {
    let trees = &device.trees;
    let changed = |node: &NodeId| {
        (device.acted.contains(node) || device.touched.contains(node))
            && !device.acked.contains(node)
    };
    let parent = |node: &NodeId| match trees.local().get(node) {
        Some(local) => Some(local.shape.parent.clone()),
        None => trees
            .remote()
            .get(node)
            .map(|remote| remote.shape.parent.clone()),
    };
    let path = |node: &NodeId| {
        let mut path = vec![node.clone()];
        while let Some(next) = parent(path.last().unwrap()).filter(|next| !path.contains(next)) {
            path.push(next);
        }
        path
    };
    let below = (trees.local().iter())
        .any(|(node, _)| changed(node) && !trees.excluded(node) && path(node).contains(id));
    let above = path(id).iter().skip(1).any(|node| moved.contains(node));
    prop_assert!(
        below || above,
        "{} is brought back from the server's trash with no change its user made in or below it and no move above it in the same commit",
        id
    );
    Ok(())
}

fn placement_check(
    device: &Device,
    record: &koidrive_engine::effects::EffectRecord,
    kept_with: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
    placed: &BTreeMap<NodeId, Position>,
    revived: &BTreeSet<NodeId>,
    merged_into: &BTreeMap<NodeId, NodeId>,
    discarded: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
) -> Result<(), TestCaseError> {
    if let (Effect::Move { .. }, Some(after), Some(local)) = (
        &record.effect,
        &record.after,
        device.trees.local().get(&record.node),
    ) {
        let mut chain = std::iter::successors(kept_with.get(&record.node), |kept| {
            kept_with.get(&kept.parent)
        })
        .take(kept_with.len() + 1);
        let taken_along = kept_with.contains_key(&record.node)
            && chain.all(|kept| !revived.contains(&kept.parent))
            && !acted_below(device, &record.node, kept_with, discarded);
        let parent = (merged_into.get(&local.shape.parent)).unwrap_or(&local.shape.parent);
        let reverted = closes_a_cycle(&device.trees, &record.node, parent)
            && (device.trees.remote().live(&record.node))
                .is_some_and(|remote| remote.same_position(after));
        let left_where_put =
            !taken_along && !reverted && placed.get(&record.node) == Some(&position(&local.shape));
        prop_assert!(
            !(left_where_put
                && after.parent != local.shape.parent
                && merged_into.get(&local.shape.parent) != Some(&after.parent)
                && revived.contains(&local.shape.parent)),
            "{} is where the user put it in a folder that is the user's again, but the effects move it out",
            record.node
        );
    }
    Ok(())
}

fn positions(trees: &Trees) -> BTreeMap<NodeId, Position> {
    (trees.local().iter())
        .map(|(id, node)| (id.clone(), position(&node.shape)))
        .collect()
}

fn acted_below(
    device: &Device,
    id: &NodeId,
    kept_with: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
    discarded: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
) -> bool {
    let trees = &device.trees;
    let held = |at: &NodeId| discarded.contains_key(at) && trees.remote().get(at).is_none();
    let reaches = |entry: &NodeId, parent: &dyn Fn(&NodeId) -> Option<NodeId>| {
        let mut seen = BTreeSet::new();
        let mut current = Some(entry.to_owned());
        while let Some(at) = current {
            if at == *id {
                return true;
            }
            if at != *entry && held(&at) || !seen.insert(at.clone()) {
                return false;
            }
            current = parent(&at);
        }
        false
    };
    let in_local = |at: &NodeId| trees.local().get(at).map(|node| node.shape.parent.clone());
    let removed_from = |at: &NodeId| in_local(at).or_else(|| device.removed_from.get(at).cloned());
    let at_discard = |at: &NodeId| {
        (kept_with.get(at).or_else(|| discarded.get(at)))
            .map(|shape| shape.parent.clone())
            .or_else(|| {
                (device.on_server_when_taken_along.get(id))
                    .and_then(|baseline| baseline.get(at))
                    .cloned()
            })
    };
    let belongs_here = |entry: &NodeId| !kept_with.contains_key(id) || reaches(entry, &at_discard);
    let as_agreed = |entry: &NodeId| {
        let Some(local) = trees.local().get(entry) else {
            return trees.remote().live(entry).is_none();
        };
        let holds = |shape: &Shape| {
            let taken_along = kept_with.get(entry).map(|kept| Shape {
                parent: kept.parent.clone(),
                name: kept.name.clone(),
                ..shape.clone()
            });
            local.shape.same(shape) || taken_along.is_some_and(|kept| local.shape.same(&kept))
        };
        let unapplied = |remote: &Shape| {
            let marker = conflict_name("", "me", NOW, 1);
            let marker = marker.strip_suffix(')').unwrap_or(&marker);
            let renamed = remote.name.match_indices(marker).any(|(at, _)| {
                let number = remote.name[at + marker.len()..]
                    .split(')')
                    .next()
                    .unwrap_or("");
                let attempt = match number.strip_prefix(' ') {
                    Some(number) => number.parse().ok(),
                    None => number.is_empty().then_some(1),
                };
                attempt.is_some_and(|attempt| {
                    remote.name == conflict_name(&local.shape.name, "me", NOW, attempt)
                })
            });
            (renamed && trees.reverted().contains(entry)).then(|| Shape {
                name: local.shape.name.clone(),
                ..remote.clone()
            })
        };
        let held_by_r = trees.remote().get(entry).is_some_and(|remote| {
            holds(unapplied(&remote.shape).as_ref().unwrap_or(&remote.shape))
        });
        held_by_r || acknowledged_shape(trees, entry).is_some_and(holds)
    };
    let kept_back = |entry: &NodeId| {
        device.kept_back.contains(entry)
            && trees.local().get(entry).is_none()
            && trees.remote().live(entry).is_some()
    };
    (device.acted.iter())
        .filter(|entry| !as_agreed(entry) && !kept_by_r7(device, entry) && !kept_back(entry))
        .any(|entry| {
            reaches(entry, &in_local) || (belongs_here(entry) && reaches(entry, &removed_from))
        })
}

fn kept_by_r7(device: &Device, entry: &NodeId) -> bool {
    let trees = &device.trees;
    let server_changed = |node: &NodeId| {
        (acknowledged_shape(trees, node)).is_some_and(|agreed| {
            (trees.remote().live(node)).is_some_and(|remote| !remote.same(agreed))
        })
    };
    let moved_by_the_user = |node: &NodeId| {
        (trees.local().get(node)).is_some_and(|local| {
            !trees.reverted().contains(node)
                && acknowledged_shape(trees, node)
                    .is_none_or(|agreed| !local.shape.same_position(agreed))
        })
    };
    let below_in_r = |node: &NodeId| {
        let mut current = trees
            .remote()
            .live(node)
            .map(|remote| remote.parent.clone());
        for _ in 0..=trees.remote().len() {
            match current {
                Some(at) if at == *entry => return true,
                Some(at) => {
                    let moved_here = (device.relocated.get(&at))
                        .is_some_and(|moved| relocation_stands(trees, &at, &moved.to));
                    if moved_here {
                        return false;
                    }
                    current = trees.remote().live(&at).map(|remote| remote.parent.clone());
                }
                None => return false,
            }
        }
        false
    };
    trees.local().get(entry).is_none()
        && trees.remote().live(entry).is_some()
        && (server_changed(entry) && !trees.excluded(entry)
            || (trees.remote().iter()).any(|(node, remote)| {
                !remote.trashed
                    && (server_changed(node) && !moved_by_the_user(node)
                        || trees.local().get(node).is_none()
                            && (!device.removed_from.contains_key(node)
                                || device.kept_back.contains(node)))
                    && !trees.excluded(node)
                    && below_in_r(node)
            }))
}

fn relocation_stands(trees: &Trees, id: &NodeId, to: &koidrive_engine::tree::Shape) -> bool {
    (trees.local().get(id)).is_some_and(|local| local.shape.same_position(to))
        && !(trees.remote().live(id)).is_some_and(|remote| remote.same_position(to))
}

fn note_agreements(
    device: &Device,
    sent_since: &mut BTreeSet<NodeId>,
    agreed: &mut BTreeMap<NodeId, ContentKey>,
    from_server: &mut BTreeSet<NodeId>,
) {
    for (id, local) in device.trees.local().iter() {
        let (Some(content), Some(remote)) = (
            &local.shape.content,
            (device.trees.remote().live(id)).and_then(|remote| remote.content.as_ref()),
        ) else {
            continue;
        };
        if !koidrive_engine::tree::same_content(content, remote) {
            continue;
        }
        let agreed_now =
            (agreed.get(id)).is_none_or(|was| !koidrive_engine::tree::same_content(was, content));
        if agreed_now && !sent_since.contains(id) {
            from_server.insert(id.clone());
        }
        sent_since.remove(id);
        agreed.insert(id.clone(), *content);
    }
}

fn note_kept_back(device: &mut Device) {
    let trees = &device.trees;
    (device.relocated).retain(|id, moved| relocation_stands(trees, id, &moved.to));
    let kept: Vec<NodeId> = (device.removed_from.keys())
        .filter(|entry| kept_by_r7(device, entry))
        .cloned()
        .collect();
    device.kept_back.extend(kept);
}

fn note_unacknowledged_taken_along_kept_back(
    device: &mut Device,
    kept_with: &BTreeMap<NodeId, koidrive_engine::tree::Shape>,
) {
    let trees = &device.trees;
    let kept: Vec<NodeId> = (kept_with.keys())
        .filter(|id| {
            device.removed_from.contains_key(*id)
                && trees.local().get(id).is_none()
                && trees.remote().live(id).is_some()
                && acknowledged_shape(trees, id).is_none()
                && !trees.excluded(id)
        })
        .cloned()
        .collect();
    device.kept_back.extend(kept);
}

/// [`run_cut`] with changes to the ignore rules: the first of `rulings` is made before the offline
/// local changes, and one more before each of the `during` changes. At the end a node the device
/// excludes, or cannot place because of an excluded one ([`waits_on_excluded`]), must have R equal
/// to the server's node, and every other node L = S = R. Every write must be on the server or on
/// the local copy of such a node, which keeps it unsent.
#[allow(clippy::too_many_arguments)]
fn run_ignoring(
    seed: &[Action],
    local: &[Action],
    remote: &[Action],
    fits: usize,
    fresh_dir_names: bool,
    during: &[(bool, Vec<Action>)],
    cut: Cut,
    rulings: &[Ruling],
    answer: Answer,
    first_link: Option<PriorSync>,
) -> Result<Ran, TestCaseError> {
    run_ignoring_answering_after(
        seed,
        local,
        remote,
        fits,
        fresh_dir_names,
        during,
        cut,
        rulings,
        answer,
        first_link,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_ignoring_answering_after(
    seed: &[Action],
    local: &[Action],
    remote: &[Action],
    fits: usize,
    fresh_dir_names: bool,
    during: &[(bool, Vec<Action>)],
    cut: Cut,
    rulings: &[Ruling],
    answer: Answer,
    first_link: Option<PriorSync>,
    answer_after: usize,
) -> Result<Ran, TestCaseError> {
    let (mut space, mut other) = seeded(seed);
    let root = space.space_id().to_owned();
    let history = space.history_id().to_owned();
    let mut device = Device::new(Trees::new(root.clone()));
    device.fresh_dir_names = fresh_dir_names;
    for state in space.nodes().iter().filter(|n| n.node_id != root) {
        let (id, node) = RemoteNode::from_state(state).unwrap();
        if !node.trashed && first_link.is_none() {
            device
                .trees
                .set_synced(id.clone(), Some(node.shape.clone()));
            let local = LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape: node.shape.clone(),
                identity: device.identity(),
                parked: false,
                chunks: node.chunks.clone(),
            };
            device.trees.set_local(id.clone(), local);
        }
        device.trees.set_remote(id, node);
    }
    let mut last_seq = space.head_seq();

    let mut commits = Commits::default();
    let (mut requested, mut sent) = (0, 0);
    let mut rulings = rulings.iter();
    if let Some(ruling) = rulings.next() {
        device.rule(ruling);
    }
    let mut placed_left_out: BTreeSet<NodeId> = BTreeSet::new();
    let mut from_server: BTreeSet<NodeId> = BTreeSet::new();
    for action in local {
        match action.kind {
            5 | 6 => {
                let asked = request_restore(&space, &mut commits, &mut device.ids, action);
                requested += usize::from(asked);
            }
            _ => act_tracked(&mut device, action, &mut placed_left_out, &mut from_server),
        }
    }
    if let Some(prior) = first_link {
        device.follow_rekeys();
        device.trees.settle(commits.aliases());
        commits.first_link(&mut device.trees, &mut device.ids, NOW as u64, prior);
    }
    let mut local_writes = written(device.trees.local().iter().map(|(_, n)| n.shape.content), 1);
    for action in remote {
        remote_action(&mut space, &mut other, 2, action);
    }
    let mut remote_writes = written(
        space
            .nodes()
            .into_iter()
            .filter(|n| n.status == 1)
            .map(|n| n.current.and_then(|v| v.content).as_ref().map(key)),
        2,
    );
    let backlog = space
        .resume(&Resume {
            history_id: history.clone(),
            since_seq: last_seq,
            ..Default::default()
        })
        .unwrap();
    for record in backlog {
        prop_assert_eq!(record.seq, last_seq + 1);
        device.receive(&record.changes, false);
        last_seq = record.seq;
    }

    let me = session("me");
    let mut effects = Effects::default();
    let (mut steps, mut quiet, mut due, mut parked_reverted) = (0, false, false, 0);
    let mut retried: Option<Trees> = None;
    let (mut deletes, mut removals) = (0, 0);
    let (mut asked, mut held_once, mut confirmed) = (0, BTreeSet::new(), BTreeSet::new());
    let mut discarded_folders = BTreeSet::new();
    let mut discarded: BTreeMap<NodeId, koidrive_engine::tree::Shape> = BTreeMap::new();
    let mut kept_with: BTreeMap<NodeId, koidrive_engine::tree::Shape> = BTreeMap::new();
    let mut placed: BTreeMap<NodeId, Position> = BTreeMap::new();
    let mut agreed: BTreeMap<NodeId, ContentKey> = BTreeMap::new();
    let mut sent_since: BTreeSet<NodeId> = BTreeSet::new();
    note_agreements(&device, &mut sent_since, &mut agreed, &mut from_server);
    let mut merged_into: BTreeMap<NodeId, NodeId> = BTreeMap::new();
    let mut failed_on_ignored: BTreeMap<NodeId, (Effect, Shape)> = BTreeMap::new();
    let mut revived_once: BTreeSet<NodeId> = BTreeSet::new();
    let mut freed: BTreeSet<NodeId> = BTreeSet::new();
    let mut sent_nodes: BTreeSet<NodeId> = BTreeSet::new();
    let mut pending: Option<(Question, BTreeMap<NodeId, koidrive_engine::tree::Shape>)> = None;
    let mut during = during.iter();
    while !quiet {
        steps += 1;
        prop_assert!(steps <= STEPS, "no convergence within {} steps", STEPS);
        device.trees.settle(commits.aliases());
        prop_assert_eq!(device.trees.check(), Ok(()));
        device.follow_rekeys();
        note_agreements(&device, &mut sent_since, &mut agreed, &mut from_server);
        if (due || steps % 2 == 0)
            && let Some((on_device, actions)) = during.next()
        {
            due = false;
            if let Some(ruling) = rulings.next() {
                device.rule(ruling);
            }
            if *on_device {
                let before = held(&device.trees);
                let was = positions(&device.trees);
                for action in actions {
                    match action.kind {
                        5 | 6 => {
                            let ids = &mut device.ids;
                            let asked = request_restore(&space, &mut commits, ids, action);
                            requested += usize::from(asked);
                        }
                        _ => {
                            act_tracked(
                                &mut device,
                                action,
                                &mut placed_left_out,
                                &mut from_server,
                            );
                        }
                    }
                }
                for (id, at) in positions(&device.trees) {
                    if was.get(&id) != Some(&at) {
                        placed.insert(id, at);
                    }
                }
                let after = held(&device.trees);
                let writes = [&mut local_writes, &mut remote_writes];
                let new = rewritten(&before, &after, &device.trees, writes);
                local_writes.extend(own(new.into_iter(), 1));
            } else {
                let before = stored(&space);
                for action in actions {
                    remote_action(&mut space, &mut other, 2, action);
                }
                let after = stored(&space);
                let writes = [&mut local_writes, &mut remote_writes];
                let new = rewritten(&before, &after, &device.trees, writes);
                remote_writes.extend(own(new.into_iter(), 2));
                let backlog = space
                    .resume(&Resume {
                        history_id: history.clone(),
                        since_seq: last_seq,
                        ..Default::default()
                    })
                    .unwrap();
                for record in backlog {
                    device.receive(&record.changes, false);
                    last_seq = record.seq;
                }
            }
        }
        device.trees.settle(commits.aliases());
        prop_assert_eq!(device.trees.check(), Ok(()));
        device.list_moved();
        device.follow_rekeys();
        (device.trees, commits, effects) =
            reload(&device.trees, &commits, &effects, &device.rules)?;
        note_kept_back(&mut device);
        note_unacknowledged_taken_along_kept_back(&mut device, &kept_with);
        let held = held_by(commits.questions());
        let revived = revive(&device, &discarded, &kept_with, &held);
        revived_once.extend(revived.iter().cloned());
        release_taken_along(&device.trees, &mut kept_with, &freed);
        let mut taken_along_now = kept_with.clone();
        let users = revived
            .iter()
            .filter(|id| !held.contains(*id))
            .cloned()
            .collect();
        release_taken_along(&device.trees, &mut taken_along_now, &users);
        let reverted = device.trees.reverted();
        parked_reverted = parked_reverted.max(
            device
                .trees
                .local()
                .iter()
                .filter(|(id, node)| node.parked && reverted.contains(*id))
                .count(),
        );
        failed_on_ignored.retain(|id, (failed, at)| {
            if effects.blocked().get(id) != Some(failed)
                || device.disk_name_taken(&at.parent, &[], &at.name)
            {
                return true;
            }
            effects.unblock(id);
            false
        });
        if let Some(record) = effects
            .plan(
                &device.trees,
                commits.in_flight(),
                &mut device.ids,
                NOW as u64,
                BatchLimit::ONE,
            )
            .first()
            .cloned()
        {
            let known = device.trees.remote().get(&record.node).is_some()
                || device.trees.synced().live(&record.node).is_some();
            let removes = match record.effect {
                Effect::RemoveFile { .. } => true,
                Effect::RemoveDir { .. } => known,
                _ => false,
            };
            removals += usize::from(removes);
            let removed = matches!(
                record.effect,
                Effect::RemoveFile { .. } | Effect::RemoveDir { .. }
            );
            let held = device.trees.remote().get(&record.node).is_some();
            let as_discarded = (device.trees.local().get(&record.node))
                .zip(discarded.get(&record.node))
                .is_some_and(|(local, shown)| {
                    local.shape.kind == Kind::File && local.shape.same(shown)
                });
            let users = revived.contains(&record.node)
                && !held
                && !sent_nodes.contains(&record.node)
                && !as_discarded;
            prop_assert!(
                !(removed && users),
                "{} is the user's again after the Discard, but the effects remove it",
                record.node
            );
            placement_check(
                &device,
                &record,
                &taken_along_now,
                &placed,
                &revived,
                &merged_into,
                &discarded,
            )?;
            if let Some(local) = (device.trees.local().get(&record.node))
                .filter(|local| local.parked || device.trees.ignored(&record.node))
            {
                let remote = device.trees.remote().live(&record.node);
                let left_or_taken_to_r = match &record.after {
                    Some(after) => {
                        after.same_position(&local.shape)
                            || remote.is_some_and(|remote| remote.same_position(after))
                    }
                    None => {
                        let discarded = device.trees.state().discarded;
                        let parked_or_taken_along =
                            std::iter::successors(Some(&record.node), |id| {
                                device.trees.local().get(id).map(|node| &node.shape.parent)
                            })
                            .any(|id| {
                                discarded.contains_key(id)
                                    || device.trees.local().get(id).is_some_and(|node| node.parked)
                            });
                        remote.is_none() && parked_or_taken_along
                    }
                };
                prop_assert!(
                    left_or_taken_to_r,
                    "the effects take the local copy of excluded or parked {} from {:?} to {:?}, and R has {:?}",
                    record.node,
                    local.shape,
                    record.after,
                    remote
                );
            }
            if let (
                Effect::CreateDir {
                    at:
                        Location::At {
                            base: Base::Root,
                            dirs,
                            name,
                        },
                }
                | Effect::WriteFile {
                    at:
                        Location::At {
                            base: Base::Root,
                            dirs,
                            name,
                        },
                    ..
                },
                Some(after),
            ) = (&record.effect, &record.after)
            {
                let path: Vec<&str> = (dirs.iter().map(|dir| dir.name.as_str()))
                    .chain([name.as_str()])
                    .collect();
                prop_assert!(
                    !device.trees.ignores(&path, after.kind),
                    "{} is written at {:?}, which the rules exclude",
                    record.node,
                    path
                );
            }
            if cut == Cut::Effect {
                (device.trees, commits, effects) =
                    reload(&device.trees, &commits, &effects, &device.rules)?;
                prop_assert_eq!(effects.in_flight().first(), Some(&record));
            }
            if let Effect::RemoveDir { along, .. } = &record.effect {
                let held = held_on_disk(&device, &record.node, along);
                if !held.is_empty() {
                    let held: Vec<(&str, Option<Kind>)> = held
                        .iter()
                        .map(|(name, kind)| (name.as_str(), *kind))
                        .collect();
                    if effects.keeps(
                        &device.trees,
                        commits.in_flight(),
                        commits.aliases(),
                        &record.effect_id,
                        &held,
                    ) {
                        effects
                            .kept(
                                &mut device.trees,
                                commits.in_flight(),
                                commits.aliases(),
                                &record.effect_id,
                                &held,
                            )
                            .unwrap();
                    } else {
                        effects
                            .failed(&mut device.trees, &record.effect_id)
                            .unwrap();
                    }
                    continue;
                }
            }
            if fail_on_ignored_name(&mut device, &mut effects, &record, &mut failed_on_ignored) {
                continue;
            }
            if matches!(record.effect, Effect::WriteFile { .. }) {
                from_server.insert(record.node.clone());
            }
            let identity = device.identity();
            effects
                .done(
                    &mut device.trees,
                    commits.in_flight(),
                    &record.effect_id,
                    Some(identity),
                )
                .unwrap();
            release_taken_along(&device.trees, &mut kept_with, &freed);
            continue;
        }
        let mut before = (device.trees.clone(), commits.questions().to_vec());
        let built = commits.build(
            &mut device.trees,
            &mut device.ids,
            NOW as u64,
            &history,
            "me",
            last_seq,
        );
        let built = built.cloned();
        if let Err(why) = commits.check_buildable() {
            panic!("{why}");
        }
        prop_assert!(
            device.trees.synced().check(Names::ByKey).is_ok(),
            "S is not a valid tree"
        );
        let open = |question: &Question| commits.questions().iter().any(|o| o.id == question.id);
        if let Some((question, at_answer)) = pending.take_if(|(question, _)| !open(question)) {
            let asked_again: BTreeSet<&NodeId> =
                (commits.questions().iter().flat_map(Question::holds)).collect();
            let shown: BTreeMap<NodeId, koidrive_engine::tree::Shape> = (question.shown)
                .into_iter()
                .filter(|(id, _)| !asked_again.contains(id))
                .collect();
            let contents = written(shown.values().map(|shape| shape.content), 1);
            local_writes.retain(|write| !contents.contains(write));
            for id in shown.keys() {
                if !device.off_disk.contains(id) {
                    device.touched.remove(id);
                    device.acted.remove(id);
                }
                placed.remove(id);
                revived_once.remove(id);
                freed.remove(id);
                sent_nodes.remove(id);
            }
            std::mem::swap(&mut device.trees, &mut before.0);
            let sets = (&from_server, &placed_left_out);
            let mut taken = taken_along(&device, &shown, sets, &discarded);
            for id in at_answer.keys() {
                let Some(local) = device.trees.local().get(id) else {
                    continue;
                };
                let mut at = local.shape.parent.clone();
                let inside = loop {
                    if shown.contains_key(&at) || at_answer.contains_key(&at) {
                        break true;
                    }
                    match device.trees.local().get(&at) {
                        Some(node) => at = node.shape.parent.clone(),
                        None => break false,
                    }
                };
                if inside {
                    taken.entry(id.clone()).or_insert(local.shape.clone());
                }
            }
            std::mem::swap(&mut device.trees, &mut before.0);
            device.acted.retain(|id| !taken.contains_key(id));
            device.note_taken_along(taken.keys());
            kept_with.append(&mut taken);
            discarded.extend(shown);
        }
        let held = held_by(commits.questions());
        let revived = revive(&device, &discarded, &kept_with, &held);
        freed.extend(revived.iter().filter(|id| !held.contains(*id)).cloned());
        revived_once.extend(revived);
        let guarded = (commits.questions().iter())
            .find(|question| !matches!(question.kind, QuestionKind::Conflict { .. }))
            .cloned();
        if let Some(question) = &guarded {
            held_once.extend(members(question));
        }
        let Some(commit) = built else {
            if let Some(question) = guarded {
                if steps < answer_after {
                    continue;
                }
                if matches!(answer, Answer::Delete | Answer::Publish) {
                    confirmed.extend(members(&question));
                    discarded_folders.retain(|id| !members(&question).any(|held| held == *id));
                }
                prop_assert!(commits.decide(&mut device.trees, &question.id, answer));
                if answer == Answer::Discard {
                    for (folder, shape) in &question.shown {
                        if shape.kind == Kind::Dir
                            && only_server_copies_below(
                                &device,
                                folder,
                                &question.shown,
                                (&from_server, &placed_left_out),
                                &discarded,
                            )
                        {
                            discarded_folders.insert(folder.clone());
                        } else {
                            discarded_folders.remove(folder);
                        }
                    }
                    let frozen = (commits.questions().iter())
                        .any(|asked| asked.id == question.id && asked.frozen.is_some());
                    let sets = (&from_server, &placed_left_out);
                    let at_answer = if frozen {
                        taken_along(&device, &question.shown, sets, &discarded)
                    } else {
                        BTreeMap::new()
                    };
                    pending = Some((question.clone(), at_answer));
                }
                asked += 1;
                continue;
            }
            quiet = (&device.trees, commits.questions()) == (&before.0, &before.1[..])
                && during.len() == 0;
            if quiet && !effects.blocked().is_empty() && retried.as_ref() != Some(&device.trees) {
                retried = Some(device.trees.clone());
                for node in effects.blocked().keys().cloned().collect::<Vec<_>>() {
                    effects.unblock(&node);
                }
                quiet = false;
            }
            continue;
        };
        let moved: BTreeSet<NodeId> = (commit.ops.iter())
            .filter_map(|op| match &op.op {
                Some(op::Op::Move(moved)) => Some(NodeId::from(&moved.node_id)),
                _ => None,
            })
            .collect();
        let mut sending = Vec::new();
        let mut positioned = Vec::new();
        for op in &commit.ops {
            let id = &NodeId::from(match &op.op {
                Some(op::Op::Delete(delete)) => &delete.node_id,
                Some(op::Op::CreateFile(create)) => &create.node_id,
                Some(op::Op::CreateDir(create)) => &create.node_id,
                Some(op::Op::Move(moved)) => &moved.node_id,
                Some(op::Op::UpdateContent(update)) => &update.node_id,
                _ => continue,
            });
            prop_assert!(
                !matches!(op.op, Some(op::Op::CreateDir(_)))
                    || !discarded_folders.contains(id)
                    || revived_once.contains(id),
                "{} was discarded and is created on the server with no later answer to publish it",
                id
            );
            if let Some(op::Op::CreateDir(_)) = &op.op
                && let Some(into) = merged_into.remove(id)
            {
                prop_assert!(
                    !before.0.reverted().contains(id),
                    "{} is sent as a new folder while it is still merged into {} and the user has not moved it out",
                    id,
                    into
                );
            }
            if let Some(op::Op::Move(_)) = &op.op
                && (device.trees.remote().get(id)).is_some_and(|node| node.trashed)
            {
                revive_check(&device, &moved, id)?;
            }
            if let Some(op::Op::Move(moved)) = &op.op
                && let Some(local) = device.trees.local().get(id).filter(|node| !node.parked)
            {
                let synced = device.trees.synced().get(id);
                let remote = device.trees.remote().get(id);
                let refused_at = if device.trees.name_left_out(id) {
                    device.trees.state().names_left_out.remove(id).flatten()
                } else {
                    None
                };
                sent_move_check(
                    moved,
                    &local.shape,
                    synced,
                    remote,
                    &me.device_id,
                    &merged_into,
                    refused_at.as_ref(),
                )?;
            }
            if matches!(
                op.op,
                Some(op::Op::CreateFile(_) | op::Op::UpdateContent(_))
            ) {
                sent_since.insert(id.clone());
            }
            sending.push(id.clone());
            if !matches!(op.op, Some(op::Op::UpdateContent(_))) {
                positioned.push(id.clone());
            }
            // A discarded entry the user changed, or a discarded folder the user put an entry in,
            // before the effects moved it to the local trash is the user's new change, which syncs
            // like any other from then on (section 5.2).
            let changed = revived_once.contains(id) || device.touched.contains(id);
            if held_once.contains(id) && !confirmed.contains(id) {
                prop_assert!(
                    discarded.contains_key(id) && changed,
                    "{} was held and sent before the person said so",
                    id
                );
                confirmed.insert(id.clone());
            }
        }
        let delete = |op: &&Op| matches!(op.op, Some(op::Op::Delete(_)));
        deletes += commit.ops.iter().filter(delete).count();
        if commit.ops.len() > fits {
            prop_assert_eq!(
                commits.failed(
                    &mut device.trees,
                    &Error {
                        code: error::Code::LimitExceeded as i32,
                        ..Default::default()
                    }
                ),
                Ok(Failure::Split)
            );
            continue;
        }
        let restores: Vec<usize> = (0..commit.ops.len())
            .filter(|index| matches!(commit.ops[*index].op, Some(op::Op::Restore(_))))
            .collect();
        let cut = if restores.is_empty() { Cut::None } else { cut };
        let in_flight = |commits: &Commits| commits.in_flight().map(|f| f.commit.clone());
        if cut == Cut::Built {
            (device.trees, commits, effects) =
                reload(&device.trees, &commits, &effects, &device.rules)?;
            prop_assert_eq!(commits.connected(&space.welcome(&me)), Reconnect::Resend);
            prop_assert_eq!(in_flight(&commits), Some(commit.clone()));
        }
        let receipts = upload(&mut space, &me, &commit.ops);
        let request = Commit {
            receipts,
            ..commit.clone()
        };
        let failed = |e| TestCaseError::fail(format!("{e:?}"));
        let bringing = restorable(&space, &commit);
        let before = stored(&space);
        let mut committed = space.commit(&me, NOW, &request).map_err(failed)?;
        placed.retain(|id, _| !positioned.contains(id));
        sent_nodes.extend(sending);
        let after = stored(&space);
        if cut == Cut::Applied {
            // The response is lost. The server counts the commit, so the device sends the same
            // commit again and gets the stored result: nothing is applied a second time.
            (device.trees, commits, effects) =
                reload(&device.trees, &commits, &effects, &device.rules)?;
            prop_assert_eq!(commits.connected(&space.welcome(&me)), Reconnect::Resend);
            prop_assert_eq!(in_flight(&commits), Some(commit.clone()));
            let head = space.head_seq();
            let again = space.commit(&me, NOW, &request).map_err(failed)?;
            prop_assert_eq!(&again.result, &committed.result);
            prop_assert_eq!((space.head_seq(), stored(&space)), (head, after.clone()));
            committed = again;
        }
        let result = committed.result;
        prop_assert_eq!(result.seq, last_seq + 1);
        for (op, outcome) in commit.ops.iter().zip(&result.results) {
            if let Some(op::Op::CreateDir(create)) = &op.op
                && outcome.code == op_result::Code::MergedInto as i32
            {
                let created = NodeId::from(&create.node_id);
                let merged = NodeId::from(&outcome.node_id);
                merged_into.insert(created.clone(), merged.clone());
                for records in [&mut discarded, &mut kept_with] {
                    if let Some(shape) = records.remove(&created) {
                        records.insert(merged.clone(), shape);
                    }
                    for shape in records.values_mut().filter(|shape| shape.parent == created) {
                        shape.parent = merged.clone();
                    }
                }
            }
        }
        let ended = commits.resolved().len();
        if cut == Cut::Answered {
            commits.received(result.clone()).unwrap();
            (device.trees, commits, effects) =
                reload(&device.trees, &commits, &effects, &device.rules)?;
            prop_assert_eq!(commits.connected(&space.welcome(&me)), Reconnect::Idle);
            prop_assert!(!commits.acknowledge(&mut device.trees, last_seq));
            prop_assert_eq!(commits.resolved().len(), ended);
            device.receive(&result.changes, true);
        } else {
            device.receive(&result.changes, true);
            commits.received(result.clone()).unwrap();
        }
        last_seq = result.seq;
        prop_assert!(commits.acknowledge(&mut device.trees, last_seq));
        device.acknowledged(&commit, &result.results);
        due = true;
        if restores.is_empty() {
            continue;
        }
        sent += restores.len();
        let ended = &commits.resolved()[ended..];
        prop_assert_eq!(ended.len(), restores.len());
        for (ended, index) in ended.iter().zip(&restores) {
            let asked = Some(op::Op::Restore(ended.intent.op().clone()));
            prop_assert_eq!(&asked, &commit.ops[*index].op);
            prop_assert_eq!(&ended.outcome, &outcome_of(&result.results[*index]));
        }
        let writes = [&mut local_writes, &mut remote_writes];
        let results = &result.results;
        restored(
            &commit,
            results,
            &bringing,
            &before,
            &after,
            &device.trees,
            writes,
        );
    }
    prop_assert!(commits.restores().is_empty(), "{:?}", commits.restores());
    prop_assert_eq!(
        (commits.resolved().len(), sent),
        (requested, requested),
        "every restore asked for ends, and its op is sent once"
    );

    device.trees.settle(commits.aliases());
    prop_assert_eq!(device.trees.check(), Ok(()));
    device.follow_rekeys();
    let kept_for_excluded = device.trees.kept_for_excluded();
    let outside = |id: &NodeId| device.trees.ignored(id) || kept_for_excluded(id);
    let unsettled: Vec<NodeId> = (device.trees.work().into_iter())
        .map(|(id, _)| id)
        .filter(|id| !outside(id))
        .collect();
    let refused_merges: BTreeSet<NodeId> = (effects.blocked().iter())
        .filter_map(|(id, effect)| {
            let Effect::RemoveDir { along, .. } = effect else {
                return None;
            };
            if !commits.aliases().contains_key(id) || !device.on_disk(id) {
                return None;
            }
            let held = held_on_disk(&device, id, along);
            let folder = local_path(&device.trees, id);
            (!held.is_empty()
                && held.iter().all(|(name, kind)| {
                    let mut entry = folder.clone();
                    entry.push(name.clone());
                    kind.is_none_or(|kind| device.trees.ignores(&entry, kind))
                }))
            .then(|| id.clone())
        })
        .collect();
    let failed_here = |id: &NodeId| {
        still_planned_target(
            &device,
            &effects,
            &failed_on_ignored,
            commits.in_flight(),
            id,
        )
    };
    for id in effects.blocked().keys() {
        prop_assert!(
            failed_here(id).is_none_or(|at| device.disk_name_taken(&at.parent, &[], &at.name)),
            "the blocked placement of {} outlived the ignored entry that took its name",
            id
        );
    }
    let blocked_by_ignored = |id: &NodeId| {
        failed_here(id).is_some_and(|at| {
            at.parent != device.trees.remote().root
                || !device.left_out_root_holds(&at.name)
                || device
                    .trees
                    .remote()
                    .live(id)
                    .is_none_or(|remote| !remote.same_position(at))
                || waits_on_left_out(&device, id)
        })
    };
    let mut waiting = refused_merges.clone();
    loop {
        let more: Vec<NodeId> = (unsettled.iter())
            .filter(|id| {
                !waiting.contains(*id)
                    && (blocked_by_ignored(id)
                        || waits_on_excluded(&device.trees, id, &waiting)
                        || waits_on_synced(&device.trees, id, &waiting)
                        || waits_on_left_out(&device, id))
            })
            .cloned()
            .collect();
        if more.is_empty() {
            break;
        }
        waiting.extend(more);
    }
    let unsettled: Vec<&NodeId> = (unsettled.iter())
        .filter(|id| !waiting.contains(*id))
        .collect();
    prop_assert_eq!(unsettled, Vec::<&NodeId>::new());
    prop_assert!(commits.problems().is_empty(), "{:?}", commits.problems());
    prop_assert!(
        (effects.blocked().keys()).all(|id| refused_merges.contains(id) || blocked_by_ignored(id)),
        "{:?}",
        effects.blocked()
    );
    let renaming = commits.renaming(&device.trees, NOW as u64);
    let reported =
        koidrive_engine::effects::stuck(&device.trees, &effects, commits.in_flight(), &renaming);
    let local = device.trees.local();
    let inside_itself = |id: &NodeId, folder: &NodeId| {
        folder == id
            || std::iter::successors(local.get(folder), |node| local.get(&node.shape.parent))
                .take(local.len())
                .take_while(|node| !node.parked)
                .any(|node| node.shape.parent == *id)
    };
    let unreachable = |id: &NodeId, shape: &koidrive_engine::tree::Shape| {
        let folder = shape.parent == local.root
            || (local.get(&shape.parent)).is_some_and(|node| node.shape.kind == Kind::Dir);
        !folder
            || inside_itself(id, &shape.parent)
            || (shape.parent == local.root && device.left_out_root_holds(&shape.name))
            || (local.iter()).any(|(other, node)| {
                other != id
                    && !node.parked
                    && node.shape.parent == shape.parent
                    && name_key(&node.shape.name) == name_key(&shape.name)
            })
    };
    let remote = device.trees.remote();
    let kept_from_remote = |id: &NodeId| {
        let mut names = Vec::new();
        let mut current = id;
        while let Some(shape) = remote.live(current).filter(|_| names.len() <= remote.len()) {
            names.push(shape.name.clone());
            current = &shape.parent;
        }
        names.reverse();
        let kind = (remote.live(id)).map_or(Kind::File, |shape| shape.kind);
        device.trees.edited(id) && *current == remote.root && device.trees.ignores(&names, kind)
    };
    let nowhere = |id: &NodeId| {
        (local.get(id)).is_some_and(|node| unreachable(id, &node.shape))
            && (remote.live(id)).is_none_or(|shape| unreachable(id, shape) || kept_from_remote(id))
    };
    let stays_parked = |id: &NodeId, kind| {
        let left_out = waits_on_left_out(&device, id);
        if kind == Kind::File && left_out {
            return reported.contains_key(id);
        }
        waiting.contains(id)
            || left_out
            || device.trees.ignored(id)
                && reported.contains_key(id)
                && (!device.trees.reverted().contains(id) || nowhere(id))
    };
    prop_assert!(
        (device.trees.local().iter())
            .all(|(id, node)| !node.parked || stays_parked(id, node.shape.kind)),
        "a parked node is neither on its way out nor reported"
    );
    let server: Vec<(NodeId, Option<koidrive_engine::tree::Shape>)> = space
        .nodes()
        .iter()
        .filter(|n| n.node_id != root)
        .map(|state| {
            let (id, node) = RemoteNode::from_state(state).unwrap();
            (id, (!node.trashed).then_some(node.shape))
        })
        .collect();
    let mut ids: BTreeSet<&NodeId> = server.iter().map(|(id, _)| id).collect();
    ids.extend(device.trees.local().iter().map(|(id, _)| id));
    ids.extend(device.trees.synced().iter().map(|(id, _)| id));
    for id in ids {
        let on_server = server
            .iter()
            .find(|(other, _)| other == id)
            .and_then(|(_, shape)| shape.as_ref());
        if outside(id) || waiting.contains(id) {
            prop_assert!(
                same(device.trees.remote().live(id), on_server),
                "R of excluded {}",
                id
            );
            continue;
        }
        let local = device.trees.local().get(id).map(|n| &n.shape);
        prop_assert!(
            same(local, on_server),
            "L of {} is {:?}, the server has {:?}",
            id,
            local,
            on_server
        );
        prop_assert!(
            same(device.trees.synced().live(id), on_server),
            "S of {}",
            id
        );
        prop_assert!(
            same(device.trees.remote().live(id), on_server),
            "R of {}",
            id
        );
    }
    local_writes_check(&device, &server, &waiting, &local_writes)?;
    let kept = written(
        server
            .iter()
            .map(|(_, s)| s.clone().and_then(|s| s.content)),
        2,
    );
    prop_assert!(remote_writes.is_subset(&kept), "a remote write was lost");
    Ok(Ran {
        parked_reverted,
        resolved: commits.resolved().to_vec(),
        deletes,
        removals,
        trashed: server.iter().filter(|(_, shape)| shape.is_none()).count(),
        asked,
        conflicts: (commits.questions().iter())
            .filter(|question| matches!(question.kind, QuestionKind::Conflict { .. }))
            .count(),
        space,
    })
}

const CUTS: [Cut; 5] = [
    Cut::None,
    Cut::Built,
    Cut::Applied,
    Cut::Answered,
    Cut::Effect,
];

fn held_on_disk(
    device: &Device,
    folder: &NodeId,
    along: &[koidrive_engine::effects::Displaced],
) -> Vec<(String, Option<Kind>)> {
    let in_l = (device.trees.local_children(folder))
        .filter(|child| device.on_disk(child))
        .filter_map(|child| device.trees.local().get(child))
        .filter(|node| !along.iter().any(|file| file.identity.same(&node.identity)))
        .map(|node| (node.shape.name.clone(), Some(node.shape.kind)));
    let ignored: BTreeMap<String, Kind> = (device.ignored.iter())
        .filter(|((holder, _), _)| holder == folder)
        .filter_map(|((_, below), entry)| match below.as_slice() {
            [name] => Some((name.clone(), entry.kind)),
            [name, ..] => Some((name.clone(), Kind::Dir)),
            [] => None,
        })
        .collect();
    in_l.chain(ignored.into_iter().map(|(name, kind)| (name, Some(kind))))
        .collect()
}

fn local_writes_check(
    device: &Device,
    server: &[(NodeId, Option<koidrive_engine::tree::Shape>)],
    waiting: &BTreeSet<NodeId>,
    local_writes: &BTreeSet<Vec<u8>>,
) -> Result<(), TestCaseError> {
    let unsent = (device.trees.local().iter())
        .filter(|(id, _)| (device.trees.ignored(id) || waiting.contains(*id)) && device.on_disk(id))
        .map(|(_, node)| node.shape.content);
    let in_ignored_folders = (device.ignored.iter())
        .filter(|((holder, below), entry)| {
            (device.disk_path(holder, below))
                .is_some_and(|path| device.trees.ignores(&path, entry.kind))
        })
        .map(|(_, entry)| entry.content);
    let kept = written(
        server
            .iter()
            .map(|(_, s)| s.clone().and_then(|s| s.content))
            .chain(unsent)
            .chain(in_ignored_folders),
        1,
    );
    prop_assert!(local_writes.is_subset(&kept), "a local write was lost");
    Ok(())
}

fn act_tracked(
    device: &mut Device,
    action: &Action,
    placed_left_out: &mut BTreeSet<NodeId>,
    from_server: &mut BTreeSet<NodeId>,
) {
    let before_action = positions(&device.trees);
    let written_before: BTreeMap<NodeId, koidrive_engine::tree::LocalNode> =
        (device.trees.local().iter())
            .map(|(id, node)| (id.clone(), node.clone()))
            .collect();
    let left_out_before: BTreeSet<NodeId> = (written_before.keys())
        .filter(|id| device.trees.excluded(id))
        .cloned()
        .collect();
    device.act(action);
    for id in left_out_before {
        if device.trees.local().get(&id).is_none() {
            device.removed_from.remove(&id);
        }
    }
    for (id, now) in device.trees.local().iter() {
        let same = (written_before.get(id)).is_some_and(|was| {
            was.identity == now.identity
                && match (&was.shape.content, &now.shape.content) {
                    (Some(was), Some(now)) => koidrive_engine::tree::same_content(was, now),
                    (was, now) => was.is_none() && now.is_none(),
                }
        });
        if !same {
            from_server.remove(id);
        }
    }
    for (id, at) in positions(&device.trees) {
        let parked = (device.trees.local().get(&id)).is_some_and(|node| node.parked);
        if before_action.get(&id) == Some(&at) || parked {
            continue;
        }
        if device.trees.excluded(&id) {
            placed_left_out.insert(id);
        } else {
            placed_left_out.remove(&id);
        }
    }
}

fn act(kind: u8, target: usize, parent: usize, name: usize, text: u8) -> Action {
    Action {
        kind,
        target,
        parent,
        name,
        text,
    }
}

fn the_server_drops_part_of_a_discarded_folder(
    before: Vec<(bool, Vec<Action>)>,
    moved: Action,
    gap: Vec<(bool, Vec<Action>)>,
    purged: Action,
    changed: Action,
    excluded: Vec<&'static str>,
) -> (Vec<(bool, Vec<Action>)>, Vec<Ruling>) {
    let during: Vec<(bool, Vec<Action>)> = (before.into_iter())
        .chain([(true, vec![moved])])
        .chain(gap)
        .chain([(false, vec![purged]), (true, vec![changed])])
        .collect();
    let rulings: Vec<Ruling> = [Ruling::File(vec![])]
        .into_iter()
        .chain((2..during.len()).map(|_| Ruling::Exclude(excluded.clone())))
        .chain([Ruling::Exclude(vec![])])
        .collect();
    (during, rulings)
}

/// The folder `a` with the file `c.txt` in it.
fn a_folder_with_a_file() -> Vec<Action> {
    vec![act(0, 0, 0, 0, 0), act(1, 0, 1, 3, 0)]
}

/// The digests of a file's versions on the server, newest first.
fn version_digests(space: &SpaceDo, node_id: &str) -> Vec<Vec<u8>> {
    let digest = |version: VersionInfo| version.content.unwrap().manifest_digest;
    versions(space, node_id).into_iter().map(digest).collect()
}
