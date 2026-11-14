// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::classify::Trees;

use koidrive_engine::commit::{Commits, Failure, Halt, MAX_OPS, Reconnect};

use koidrive_engine::effects::{Base, BatchLimit, Effect, Effects, Location};

use koidrive_engine::ids::IdGen;

use koidrive_engine::ignore::{LocalRules, RuleFile};

use koidrive_engine::names::name_key;

use koidrive_engine::question::{Answer, Question, QuestionKind};

use koidrive_engine::restore::{Outcome, Rejection, Resolved, RestoreIntent};

use koidrive_engine::state::StateError;

use koidrive_engine::tree::{FileIdentity, Kind, LocalNode, Names, RemoteNode, same};

use koidrive_proto::v1::{
    ChangeRecord, ChunkRef, Commit, Content, CreateDir, CreateFile, Delete, Error, ListVersions,
    Move, NodeState, Op, OpResult, PrepareUpload, Restore, Resume, UpdateContent, UploadReceipt,
    VersionInfo, change_record, chunk_plan, error, op, op_result,
};

use koidrive_sim::space::{Role, Session, SpaceDo};

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
        }),
        1 => op::Op::CreateFile(CreateFile {
            node_id: ids.uuid_v7(NOW as u64),
            parent_id: parent,
            name,
            content: Some(content(device, action.text)),
        }),
        2 => match target(&files) {
            Some(node_id) => op::Op::UpdateContent(UpdateContent {
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
            }),
            None => return,
        },
    };
    let session = session("other");
    let ops = vec![Op { op: Some(body) }];
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
        node.map(|node| RestoreIntent::from_trash(node, parent, name))
    } else {
        let status = if action.text == 2 { 2 } else { 1 };
        target(&nodes_with(space, status, Some(2))).map(|node| {
            let versions = versions(space, &node);
            let version = match unknown {
                true => ids.uuid_v7(NOW as u64),
                false => versions[action.name % versions.len()].version_id.clone(),
            };
            RestoreIntent::version(node, version)
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
            node: result.node_id.clone(),
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
            let node = if copied { &result.node_id } else { node };
            let on_disk = (trees.local().get(node)).and_then(|n| n.shape.content.as_ref());
            let edited = from_trash
                && on_disk.is_some_and(|c| c.manifest_digest != *digest)
                && acknowledged(trees, node).as_ref() == Some(digest);
            if after.contains(&(node.clone(), digest.clone())) && !edited {
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
        dev: 1,
        ino: ino.into(),
        birth_ns: ino.is_multiple_of(2).then_some(1_700_000_000_000_000_000),
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
            Some(change_record::Change::Purged(purged)) => trees.remove_remote(&purged.node_id),
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
        trees.set_rules(&root, (!self.file.is_empty()).then_some(file));
        trees.set_local_rules(LocalRules::new(&self.include, &self.exclude).unwrap());
        trees.rules_scanned();
    }
}

/// The names from the root to `id` in L.
fn local_path(trees: &Trees, id: &str) -> Vec<String> {
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
    touched: BTreeSet<String>,
}

impl Device {
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
        let hidden = self.trees.ignored_local();
        let visible = |id: &str| {
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
        };
        let nodes: Vec<(String, LocalNode)> = local
            .iter()
            .filter(|(id, _)| visible(id))
            .map(|(id, node)| (id.clone(), node.clone()))
            .collect();
        let dirs: Vec<String> = [root.clone()]
            .into_iter()
            .chain(
                nodes
                    .iter()
                    .filter(|(_, n)| n.shape.kind == Kind::Dir)
                    .map(|(id, _)| id.clone()),
            )
            .collect();
        let parent = dirs[action.parent % dirs.len()].clone();
        let unique = format!("local-{}", self.inos);
        let name = if action.kind == 0 && self.fresh_dir_names {
            unique.as_str()
        } else {
            NAMES[action.name]
        };
        let taken = |id: &str| {
            local.iter().any(|(other, n)| {
                other != id
                    && !n.parked
                    && n.shape.parent == parent
                    && name_key(&n.shape.name) == name_key(name)
            })
        };
        let target = (!nodes.is_empty()).then(|| nodes[action.target % nodes.len()].clone());
        let unseen = |id: &str, kind| self.trees.ignores(&local_path(&self.trees, id), kind);
        let into_ignored = parent != root && unseen(&parent, Kind::Dir);
        let lands_unseen = |kind| {
            let mut path = local_path(&self.trees, &parent);
            path.push(name.to_owned());
            into_ignored || self.trees.ignores(&path, kind)
        };
        match (action.kind, target) {
            (0 | 1, _) if !taken("") => {
                let (kind, content) = match action.kind {
                    0 => (Kind::Dir, None),
                    _ => (Kind::File, Some(content(1, action.text))),
                };
                if lands_unseen(kind) {
                    return;
                }
                let shape = koidrive_engine::tree::Shape {
                    parent,
                    name: name.into(),
                    kind,
                    content,
                };
                let node = LocalNode {
                    shape,
                    identity: self.identity(),
                    parked: false,
                };
                self.trees.set_local(self.ids.uuid_v7(NOW as u64), node);
            }
            (2, Some((id, mut node))) if node.shape.kind == Kind::File => {
                if unseen(&id, Kind::File) {
                    return;
                }
                let written = Some(content(1, action.text));
                if node.shape.content != written {
                    self.touched.insert(id.clone());
                }
                node.shape.content = written;
                self.trees.set_local(id, node);
            }
            (3, Some((id, mut node))) if !taken(&id) && !self.holds(&id, &parent) => {
                let kind = node.shape.kind;
                match (unseen(&id, kind), lands_unseen(kind)) {
                    (true, true) => {}
                    (false, true) if into_ignored => {
                        self.touched.insert(id.clone());
                        self.trees.ignore_local(&id)
                    }
                    _ => {
                        if node.shape.parent != parent || node.shape.name != name {
                            self.touched.insert(id.clone());
                        }
                        node.shape.parent = parent;
                        node.shape.name = name.into();
                        self.trees.set_local(id, node);
                    }
                }
            }
            (4, Some((id, node))) if !unseen(&id, node.shape.kind) => {
                let mut doomed = vec![id];
                let mut held = Vec::new();
                let mut next = 0;
                while let Some(current) = doomed.get(next).cloned() {
                    for child in self.trees.local_children(&current) {
                        match self.trees.ignored_local().contains(child) {
                            true => held.push(child.clone()),
                            false => doomed.push(child.clone()),
                        }
                    }
                    next += 1;
                }
                let mut kept = BTreeSet::new();
                for node in &held {
                    let mut above = self.trees.local().get(node).map(|n| n.shape.parent.clone());
                    while let Some(folder) = above.filter(|folder| doomed.contains(folder)) {
                        if !kept.insert(folder.clone()) {
                            break;
                        }
                        above = self
                            .trees
                            .local()
                            .get(&folder)
                            .map(|n| n.shape.parent.clone());
                    }
                }
                doomed.retain(|node| !kept.contains(node));
                for folder in &kept {
                    self.trees.ignore_local(folder);
                }
                for other in doomed {
                    self.trees.remove_local(&other);
                }
            }
            _ => {}
        }
    }

    /// True when `ancestor` is `id` or holds it in L.
    fn holds(&self, ancestor: &str, id: &str) -> bool {
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

fn written(contents: impl Iterator<Item = Option<Content>>, device: u8) -> BTreeSet<Vec<u8>> {
    own(contents.flatten().map(|c| c.manifest_digest), device)
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
fn acknowledged(trees: &Trees, id: &str) -> Option<Vec<u8>> {
    let shape = match trees.waiting().get(id) {
        Some(waiting) => waiting.shape.as_ref(),
        None => trees.synced().live(id),
    }?;
    Some(shape.content.as_ref()?.manifest_digest.clone())
}

fn held(trees: &Trees) -> BTreeSet<(String, Vec<u8>)> {
    trees
        .local()
        .iter()
        .filter_map(|(id, node)| {
            Some((
                id.clone(),
                node.shape.content.as_ref()?.manifest_digest.clone(),
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
        .filter(|(id, digest)| acknowledged(trees, id).as_ref() != Some(digest))
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
fn members(question: &Question) -> impl Iterator<Item = String> + '_ {
    (question.held.values()).flat_map(|held| held.members.keys().cloned())
}

/// Whether the effects cannot place `id` because of an excluded node: on the way from the node's
/// R position to the root, a name is held in L by an excluded node, whose local copy stays where
/// it is, or by a node that waits the same way, or a folder on the way waits itself or is
/// excluded and not where R has it. A folder R
/// no longer holds waits when L still holds such a node in it, since only an empty folder is
/// removed. Such a node stays as it is, and the driver reports it as a problem (section 5.1).
fn waits_on_excluded(trees: &Trees, id: &str, waiting: &BTreeSet<String>) -> bool {
    let mut current = id;
    for _ in 0..=trees.remote().len() {
        let Some(remote) = trees.remote().live(current) else {
            let stays = |other: &str| trees.ignored(other) || waiting.contains(other);
            return current == id
                && (trees.local().iter())
                    .any(|(other, node)| node.shape.parent == id && stays(other));
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

/// Whether S cannot take `id` at its R position because the S entry of a node that waits still
/// holds that name. The entry moves only once its node is placed, which the excluded node keeps
/// from happening.
fn waits_on_synced(trees: &Trees, id: &str, waiting: &BTreeSet<String>) -> bool {
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
        false,
    )
}

fn server_copy(
    trees: &Trees,
    id: &str,
    kept: &mut BTreeMap<String, koidrive_engine::tree::Shape>,
) -> bool {
    let (Some(local), Some(remote)) = (trees.local().get(id), trees.remote().get(id)) else {
        return false;
    };
    let mut below = BTreeMap::new();
    let copy = match (&local.shape.content, &remote.shape.content) {
        (Some(local), Some(remote)) => koidrive_engine::tree::same_content(local, remote),
        (None, None) => (trees.local_children(id)).all(|child| server_copy(trees, child, &mut below)),
        _ => false,
    };
    if copy {
        kept.append(&mut below);
        kept.insert(id.to_owned(), local.shape.clone());
    }
    copy
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
    first_link: bool,
) -> Result<Ran, TestCaseError> {
    let (mut space, mut other) = seeded(seed);
    let root = space.space_id().to_owned();
    let history = space.history_id().to_owned();
    let mut device = Device {
        trees: Trees::new(root.clone()),
        ids: IdGen::new(7),
        inos: 0,
        fresh_dir_names,
        rules: Rules::default(),
        touched: BTreeSet::new(),
    };
    for state in space.nodes().iter().filter(|n| n.node_id != root) {
        let (id, node) = RemoteNode::from_state(state).unwrap();
        if !node.trashed && !first_link {
            device
                .trees
                .set_synced(id.clone(), Some(node.shape.clone()));
            let local = LocalNode {
                shape: node.shape.clone(),
                identity: device.identity(),
                parked: false,
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
    for action in local {
        match action.kind {
            5 | 6 => {
                let asked = request_restore(&space, &mut commits, &mut device.ids, action);
                requested += usize::from(asked);
            }
            _ => device.act(action),
        }
    }
    if first_link {
        device.trees.settle();
        commits.first_link(&mut device.trees, &mut device.ids, NOW as u64);
    }
    let mut local_writes = written(
        device
            .trees
            .local()
            .iter()
            .map(|(_, n)| n.shape.content.clone()),
        1,
    );
    for action in remote {
        remote_action(&mut space, &mut other, 2, action);
    }
    let mut remote_writes = written(
        space
            .nodes()
            .into_iter()
            .filter(|n| n.status == 1)
            .map(|n| n.current.and_then(|v| v.content)),
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
        apply(&mut device.trees, &record.changes);
        last_seq = record.seq;
    }

    let me = session("me");
    let mut effects = Effects::default();
    let (mut steps, mut quiet, mut due, mut parked_reverted) = (0, false, false, 0);
    let (mut deletes, mut removals) = (0, 0);
    let (mut asked, mut held_once, mut confirmed) = (0, BTreeSet::new(), BTreeSet::new());
    let mut discarded: BTreeMap<String, koidrive_engine::tree::Shape> = BTreeMap::new();
    let mut kept_with: BTreeMap<String, koidrive_engine::tree::Shape> = BTreeMap::new();
    let mut revived: BTreeSet<String> = BTreeSet::new();
    let mut pending: Option<Question> = None;
    let mut during = during.iter();
    while !quiet {
        steps += 1;
        prop_assert!(steps <= STEPS, "no convergence within {} steps", STEPS);
        device.trees.settle();
        if (due || steps % 2 == 0)
            && let Some((on_device, actions)) = during.next()
        {
            due = false;
            if let Some(ruling) = rulings.next() {
                device.rule(ruling);
            }
            if *on_device {
                let before = held(&device.trees);
                for action in actions {
                    match action.kind {
                        5 | 6 => {
                            let ids = &mut device.ids;
                            let asked = request_restore(&space, &mut commits, ids, action);
                            requested += usize::from(asked);
                        }
                        _ => device.act(action),
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
                    apply(&mut device.trees, &record.changes);
                    last_seq = record.seq;
                }
            }
        }
        device.trees.settle();
        prop_assert_eq!(device.trees.check(), Ok(()));
        (device.trees, commits, effects) =
            reload(&device.trees, &commits, &effects, &device.rules)?;
        let reverted = device.trees.reverted();
        parked_reverted = parked_reverted.max(
            device
                .trees
                .local()
                .iter()
                .filter(|(id, node)| node.parked && reverted.contains(*id))
                .count(),
        );
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
            let identity = device.identity();
            effects
                .done(
                    &mut device.trees,
                    commits.in_flight(),
                    &record.effect_id,
                    Some(identity),
                )
                .unwrap();
            continue;
        }
        let before = device.trees.clone();
        let built = commits.build(
            &mut device.trees,
            &mut device.ids,
            NOW as u64,
            &history,
            last_seq,
        );
        let built = built.cloned();
        prop_assert!(
            device.trees.synced().check(Names::ByKey).is_ok(),
            "S is not a valid tree"
        );
        let open = |question: &Question| commits.questions().iter().any(|o| o.id == question.id);
        if let Some(question) = pending.take_if(|question| !open(question)) {
            let asked_again: BTreeSet<&String> =
                (commits.questions().iter().flat_map(Question::holds)).collect();
            let shown: BTreeMap<String, koidrive_engine::tree::Shape> = (question.shown)
                .into_iter()
                .filter(|(id, _)| !asked_again.contains(id))
                .collect();
            let contents = written(shown.values().map(|shape| shape.content.clone()), 1);
            local_writes.retain(|write| !contents.contains(write));
            for id in shown.keys() {
                device.touched.remove(id);
                revived.remove(id);
            }
            for folder in shown.keys() {
                for child in device.trees.local_children(folder) {
                    if !shown.contains_key(child) && device.trees.excluded(child) {
                        server_copy(&device.trees, child, &mut kept_with);
                    }
                }
            }
            discarded.extend(shown);
        }
        // A discarded entry the user changed, or a discarded folder that holds an entry the answer
        // did not discard there, is the user's, and so is every discarded folder above it.
        let trees = &device.trees;
        let reclaimed = |id: &&String| {
            let changed = device.touched.contains(*id) && trees.local().get(id).is_some();
            changed
                || (trees.local_children(id)).any(|child| {
                    !trees.excluded(child)
                        && kept_with.get(child) != trees.local().get(child).map(|node| &node.shape)
                        && discarded
                            .get(child)
                            .is_none_or(|shown| &shown.parent != *id)
                })
        };
        for id in discarded.keys().filter(reclaimed) {
            let mut next = Some(id.clone());
            while let Some(id) = next.take_if(|id| discarded.contains_key(id)) {
                next = (trees.local().get(&id)).map(|node| node.shape.parent.clone());
                revived.insert(id);
            }
        }
        let guarded = (commits.questions().iter())
            .find(|question| !matches!(question.kind, QuestionKind::Conflict { .. }))
            .cloned();
        if let Some(question) = &guarded {
            held_once.extend(members(question));
        }
        let Some(commit) = built else {
            if let Some(question) = guarded {
                if matches!(answer, Answer::Delete | Answer::Publish) {
                    confirmed.extend(members(&question));
                }
                prop_assert!(commits.decide(&question.id, answer));
                if answer == Answer::Discard {
                    pending = Some(question.clone());
                }
                asked += 1;
                continue;
            }
            quiet = device.trees == before && during.len() == 0;
            continue;
        };
        for op in &commit.ops {
            let id = match &op.op {
                Some(op::Op::Delete(delete)) => &delete.node_id,
                Some(op::Op::CreateFile(create)) => &create.node_id,
                Some(op::Op::CreateDir(create)) => &create.node_id,
                Some(op::Op::Move(moved)) => &moved.node_id,
                Some(op::Op::UpdateContent(update)) => &update.node_id,
                _ => continue,
            };
            // A discarded entry the user changed, or a discarded folder the user put an entry in,
            // before the effects moved it to the local trash is the user's new change, which syncs
            // like any other from then on (section 5.2).
            let changed = revived.contains(id) || device.touched.contains(id);
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
                commits.failed(&Error {
                    code: error::Code::LimitExceeded as i32,
                    ..Default::default()
                }),
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
        let ended = commits.resolved().len();
        if cut == Cut::Answered {
            commits.received(result.clone()).unwrap();
            (device.trees, commits, effects) =
                reload(&device.trees, &commits, &effects, &device.rules)?;
            prop_assert_eq!(commits.connected(&space.welcome(&me)), Reconnect::Idle);
            prop_assert!(!commits.acknowledge(&mut device.trees, last_seq));
            prop_assert_eq!(commits.resolved().len(), ended);
            apply(&mut device.trees, &result.changes);
        } else {
            apply(&mut device.trees, &result.changes);
            commits.received(result.clone()).unwrap();
        }
        last_seq = result.seq;
        prop_assert!(commits.acknowledge(&mut device.trees, last_seq));
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

    device.trees.settle();
    let kept_for_excluded = device.trees.kept_for_excluded();
    let outside = |id: &str| device.trees.ignored(id) || kept_for_excluded(id);
    let unsettled: Vec<String> = (device.trees.work().into_iter())
        .map(|(id, _)| id)
        .filter(|id| !outside(id))
        .collect();
    let mut waiting: BTreeSet<String> = BTreeSet::new();
    loop {
        let more: Vec<String> = (unsettled.iter())
            .filter(|id| {
                !waiting.contains(*id)
                    && (waits_on_excluded(&device.trees, id, &waiting)
                        || waits_on_synced(&device.trees, id, &waiting))
            })
            .cloned()
            .collect();
        if more.is_empty() {
            break;
        }
        waiting.extend(more);
    }
    let unsettled: Vec<&String> = (unsettled.iter())
        .filter(|id| !waiting.contains(*id))
        .collect();
    prop_assert_eq!(unsettled, Vec::<&String>::new());
    prop_assert!(commits.problems().is_empty(), "{:?}", commits.problems());
    prop_assert!(effects.blocked().is_empty(), "{:?}", effects.blocked());
    let reported = koidrive_engine::effects::stuck(&device.trees, &effects, commits.in_flight());
    let local = device.trees.local();
    let unreachable = |id: &str, shape: &koidrive_engine::tree::Shape| {
        let folder = shape.parent == local.root
            || (local.get(&shape.parent)).is_some_and(|node| node.shape.kind == Kind::Dir);
        !folder
            || (local.iter()).any(|(other, node)| {
                other != id
                    && !node.parked
                    && node.shape.parent == shape.parent
                    && name_key(&node.shape.name) == name_key(&shape.name)
            })
    };
    let remote = device.trees.remote();
    let kept_from_remote = |id: &str| {
        let mut names = Vec::new();
        let mut current = id;
        while let Some(shape) = remote.live(current).filter(|_| names.len() <= remote.len()) {
            names.push(shape.name.clone());
            current = &shape.parent;
        }
        names.reverse();
        let kind = (remote.live(id)).map_or(Kind::File, |shape| shape.kind);
        device.trees.edited(id) && current == remote.root && device.trees.ignores(&names, kind)
    };
    let nowhere = |id: &str| {
        (local.get(id)).is_some_and(|node| unreachable(id, &node.shape))
            && (remote.live(id)).is_none_or(|shape| unreachable(id, shape) || kept_from_remote(id))
    };
    let stays_parked = |id: &str| {
        waiting.contains(id)
            || device.trees.ignored(id)
                && reported.contains_key(id)
                && (!device.trees.reverted().contains(id) || nowhere(id))
    };
    prop_assert!(
        (device.trees.local().iter()).all(|(id, node)| !node.parked || stays_parked(id)),
        "a parked node is neither on its way out nor reported"
    );
    let server: Vec<(String, Option<koidrive_engine::tree::Shape>)> = space
        .nodes()
        .iter()
        .filter(|n| n.node_id != root)
        .map(|state| {
            let (id, node) = RemoteNode::from_state(state).unwrap();
            (id, (!node.trashed).then_some(node.shape))
        })
        .collect();
    let mut ids: BTreeSet<&String> = server.iter().map(|(id, _)| id).collect();
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
    let unsent = (device.trees.local().iter())
        .filter(|(id, _)| device.trees.ignored(id) || waiting.contains(*id))
        .map(|(_, node)| node.shape.content.clone());
    let kept = written(
        server
            .iter()
            .map(|(_, s)| s.clone().and_then(|s| s.content))
            .chain(unsent),
        1,
    );
    prop_assert!(local_writes.is_subset(&kept), "a local write was lost");
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

fn act(kind: u8, target: usize, parent: usize, name: usize, text: u8) -> Action {
    Action {
        kind,
        target,
        parent,
        name,
        text,
    }
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
