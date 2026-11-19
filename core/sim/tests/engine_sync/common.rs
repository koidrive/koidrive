// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::classify::{Pending, Trees};

use koidrive_engine::commit::{Commits, Failure, Halt, MAX_OPS, Reconnect};

use koidrive_engine::effects::{Base, BatchLimit, Effect, Effects, Location};

use koidrive_engine::first_link::PriorSync;

use koidrive_engine::ids::IdGen;

use koidrive_engine::ignore::{LocalRules, RuleFile, Source};

use koidrive_engine::names::name_key;

use koidrive_engine::question::{Answer, Question, QuestionKind};

use koidrive_engine::restore::{Outcome, Rejection, Resolved, RestoreIntent};

use koidrive_engine::state::StateError;

use koidrive_engine::tree::{FileIdentity, Kind, LocalNode, Names, RemoteNode, Shape, same};

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
            pending_for: String::new(),
        }),
        1 => op::Op::CreateFile(CreateFile { made_over: Vec::new(), source_version_id: String::new(),
            node_id: ids.uuid_v7(NOW as u64),
            parent_id: parent,
            name,
            content: Some(content(device, action.text)),
            ..Default::default()
        }),
        2 => match target(&files) {
            Some(node_id) => op::Op::UpdateContent(UpdateContent { made_over: Vec::new(), source_version_id: String::new(),
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
    let ops = vec![Op { op: Some(body), ..Default::default() }];
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
    acted: BTreeSet<String>,
    removed_from: BTreeMap<String, String>,
    kept_back: BTreeSet<String>,
    relocated: BTreeMap<String, Relocation>,
}

struct Relocation {
    from: koidrive_engine::tree::Shape,
    to: koidrive_engine::tree::Shape,
}

impl Device {
    fn relocate(
        &mut self,
        id: &str,
        before: &koidrive_engine::tree::Shape,
        after: &koidrive_engine::tree::Shape,
    ) {
        let from = (self.relocated.remove(id))
            .filter(|moved| moved.to.same_position(before))
            .map_or_else(|| before.clone(), |moved| moved.from);
        if !from.same_position(after) {
            let to = after.clone();
            self.relocated.insert(id.to_owned(), Relocation { from, to });
        }
    }

    fn receive(&mut self, changes: &[ChangeRecord], answered: bool) {
        for change in changes {
            apply(&mut self.trees, std::slice::from_ref(change));
            let Some(change_record::Change::Node(state)) = &change.change else {
                continue;
            };
            let Some(remote) = self.trees.remote().live(&state.node_id).cloned() else {
                continue;
            };
            let Some(moved) = self.relocated.get_mut(&state.node_id) else {
                continue;
            };
            if remote.same_position(&moved.to) {
                self.relocated.remove(&state.node_id);
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
        self.rescan();
    }

    fn rescan(&mut self) {
        let local = self.trees.local();
        let folders: Vec<String> = std::iter::once(local.root.clone())
            .chain(
                (local.iter())
                    .filter(|(id, node)| node.shape.kind == Kind::Dir && visible(&self.trees, id))
                    .map(|(id, _)| id.clone()),
            )
            .collect();
        self.trees.listed(&folders, Pending::default());
    }

    fn list_moved(&mut self) {
        let moved: Vec<String> = (self.trees.take_relist().into_iter())
            .filter(|id| visible(&self.trees, id))
            .collect();
        self.trees.listed(&moved, Pending::default());
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
        let nodes: Vec<(String, LocalNode)> = local
            .iter()
            .filter(|(id, _)| visible(&self.trees, id))
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
                    left_out_when_placed: false,
                    content_from_server: false,
                    shape,
                    identity: self.identity(),
                    parked: false,
                };
                let id = self.ids.uuid_v7(NOW as u64);
                self.acted.insert(id.clone());
                self.trees.set_local_changed(id, node);
            }
            (2, Some((id, mut node))) if node.shape.kind == Kind::File => {
                if unseen(&id, Kind::File) {
                    return;
                }
                let written = Some(content(1, action.text));
                if node.shape.content != written {
                    self.touched.insert(id.clone());
                    self.acted.insert(id.clone());
                }
                node.shape.content = written;
                self.trees.set_local_changed(id, node);
            }
            (3, Some((id, mut node))) if !taken(&id) && !self.holds(&id, &parent) => {
                let kind = node.shape.kind;
                match (unseen(&id, kind), lands_unseen(kind)) {
                    (true, true) => {}
                    (false, true) if into_ignored => {
                        self.touched.insert(id.clone());
                        self.acted.insert(id.clone());
                        self.trees.ignore_local(&id)
                    }
                    _ => {
                        let before = node.shape.clone();
                        node.shape.parent = parent;
                        node.shape.name = name.into();
                        if before.parent != node.shape.parent || before.name != node.shape.name {
                            self.touched.insert(id.clone());
                            self.acted.insert(id.clone());
                            self.relocate(&id, &before, &node.shape);
                        }
                        self.trees.set_local_changed(id, node);
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
                    self.acted.insert(folder.clone());
                    self.trees.ignore_local(folder);
                }
                for other in doomed.into_iter().rev() {
                    if let Some(node) = self.trees.local().get(&other) {
                        self.removed_from.insert(other.clone(), node.shape.parent.clone());
                    }
                    self.kept_back.remove(&other);
                    self.relocated.remove(&other);
                    self.acted.insert(other.clone());
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

fn visible(trees: &Trees, id: &str) -> bool {
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
    nodes: &[(String, LocalNode)],
    dirs: &[String],
    action: &Action,
) -> Option<Action> {
    let ids: Vec<&String> = nodes.iter().map(|(id, _)| id).collect();
    let at = |id: &str| ids.iter().position(|other| *other == id);
    let dir_at = |id: &str| dirs.iter().position(|other| other == id);
    let pick = |list: Vec<String>| (!list.is_empty()).then(|| list[action.target % list.len()].clone());
    let server_dir = |id: &String| (trees.remote().live(id)).is_some_and(|r| r.kind == Kind::Dir);
    let local_only = |id: &String| *id != trees.local().root && trees.remote().get(id).is_none();
    let above = |id: &str| {
        let mut parents = Vec::new();
        let mut current = trees.local().get(id).map(|node| node.shape.parent.clone());
        while let Some(parent) = current.filter(|_| parents.len() <= trees.local().len()) {
            current = trees.local().get(&parent).map(|node| node.shape.parent.clone());
            parents.push(parent);
        }
        parents
    };
    if action.kind == 8 {
        let target = pick(dirs.iter().filter(|dir| server_dir(dir)).cloned().collect())?;
        let parents: Vec<&String> = (dirs.iter())
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
    let folders: Vec<String> = (dirs.iter())
        .filter(|dir| server_dir(dir) && above(dir).iter().any(local_only))
        .cloned()
        .collect();
    let folder = pick(folders)?;
    let below = |kind: Option<Kind>| -> Vec<String> {
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
    let shape = acknowledged_shape(trees, id)?;
    Some(shape.content.as_ref()?.manifest_digest.clone())
}

fn acknowledged_shape<'a>(trees: &'a Trees, id: &str) -> Option<&'a Shape> {
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
fn members(question: &Question) -> impl Iterator<Item = String> + '_ {
    (question.held.values()).flat_map(|held| held.members.keys().cloned())
}

fn held_by(questions: &[Question]) -> BTreeSet<String> {
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
fn waits_on_excluded(trees: &Trees, id: &str, waiting: &BTreeSet<String>) -> bool {
    let mut current = id;
    for _ in 0..=trees.remote().len() {
        let Some(remote) = trees.remote().live(current) else {
            let stays = |other: &str| trees.ignored(other) || waiting.contains(other);
            return current == id
                && ((trees.local().iter())
                    .any(|(other, node)| node.shape.parent == id && stays(other))
                    || (trees.synced().iter())
                        .any(|(other, shape)| shape.parent == id && waiting.contains(other)));
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
        None,
    )
}

fn still_discarded_in(
    device: &Device,
    id: &str,
    folder: &str,
    discarded: &BTreeMap<String, koidrive_engine::tree::Shape>,
) -> bool {
    let trees = &device.trees;
    (discarded.get(id))
        .zip(trees.local().get(id))
        .is_some_and(|(shown, local)| shown.parent == folder && local.shape.same(shown))
        && !removed_by_user_from(device, id)
        && (trees.local().iter())
            .filter(|(_, node)| node.shape.parent == id)
            .all(|(child, _)| still_discarded_in(device, child, id, discarded))
}

fn removed_by_user_from(device: &Device, id: &str) -> bool {
    let trees = &device.trees;
    (trees.remote().iter()).any(|(child, node)| {
        !node.trashed
            && node.shape.parent == id
            && trees.local().get(child).is_none()
            && (device
                .removed_from
                .get(child)
                .is_some_and(|from| from == id)
                || device.removed_from.contains_key(child)
                    && acknowledged_shape(trees, child).is_some_and(|agreed| agreed.parent == id))
            && acknowledged_shape(trees, child).is_some()
            && !kept_by_r7(device, child)
    })
}

fn server_copy(
    device: &Device,
    id: &str,
    shown: &BTreeMap<String, koidrive_engine::tree::Shape>,
    from_server: &BTreeSet<String>,
    discarded: &BTreeMap<String, koidrive_engine::tree::Shape>,
    kept: &mut BTreeMap<String, koidrive_engine::tree::Shape>,
) -> bool {
    let trees = &device.trees;
    let (Some(local), Some(remote)) = (trees.local().get(id), trees.remote().get(id)) else {
        return false;
    };
    let mut below = BTreeMap::new();
    let copy = match (&local.shape.content, &remote.shape.content) {
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

fn revive(
    device: &Device,
    discarded: &BTreeMap<String, koidrive_engine::tree::Shape>,
    kept_with: &BTreeMap<String, koidrive_engine::tree::Shape>,
    asked: &BTreeSet<String>,
) -> BTreeSet<String> {
    // A discarded entry the user changed, or a discarded folder that holds an entry the answer
    // did not discard there, is the user's, and so is every discarded folder above it.
    let trees = &device.trees;
    let reclaimed = |id: &&String| {
        let changed = device.touched.contains(*id)
            && !trees.excluded(id)
            && (trees.local().get(id))
                .is_some_and(|local| !discarded.get(*id).is_some_and(|shown| local.shape.same(shown)));
        changed
            || (trees.local_children(id)).any(|child| {
                !trees.excluded(child)
                    && !asked.contains(child)
                    && (!kept_with.contains_key(child)
                        || acted_below(device, child, kept_with))
                    && discarded
                        .get(child)
                        .is_none_or(|shown| &shown.parent != *id)
            })
    };
    let mut revived = BTreeSet::new();
    let unsent = |id: &&String| trees.remote().get(id).is_none();
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
    kept_with: &mut BTreeMap<String, Shape>,
    revived: &BTreeSet<String>,
) {
    let reclaimed = |folder: &String| {
        revived.contains(folder)
            && (trees.synced().get(folder).is_none() || trees.remote().get(folder).is_some())
    };
    let home = |id: &String| {
        (trees.local().get(id)).is_some_and(|local| {
            !local.parked
                && (trees.remote().live(id)).is_some_and(|remote| remote.same_position(&local.shape))
        })
    };
    let released: Vec<String> = (kept_with.iter())
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

type Position = (String, String);

fn closes_a_cycle(trees: &Trees, id: &str, parent: &str) -> bool {
    let mut folder = Some(parent.to_owned());
    for _ in 0..=trees.local().len() + trees.remote().len() {
        match folder {
            Some(at) if at == id => return true,
            Some(at) => {
                folder = (trees.remote().get(&at).map(|node| node.shape.parent.clone()))
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
    merged_into: &BTreeMap<String, String>,
) -> Result<(), TestCaseError> {
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
    let from = (moved.from.as_ref()).map(|from| (&from.parent_id, &from.name));
    if finishes_mark && from == Some((parent, &local.name)) {
        return Ok(());
    }
    let name = Some(&moved.pending_for)
        .filter(|name| !name.is_empty())
        .unwrap_or(&moved.new_name);
    prop_assert_eq!(
        (&moved.new_parent_id, name),
        (parent, &local.name),
        "{} is sent to a place the disk does not hold it at",
        moved.node_id
    );
    Ok(())
}

fn placement_check(
    device: &Device,
    record: &koidrive_engine::effects::EffectRecord,
    kept_with: &BTreeMap<String, koidrive_engine::tree::Shape>,
    placed: &BTreeMap<String, Position>,
    revived: &BTreeSet<String>,
    merged_into: &BTreeMap<String, String>,
) -> Result<(), TestCaseError> {
    if let (Effect::Move { .. }, Some(after), Some(local)) = (
        &record.effect,
        &record.after,
        device.trees.local().get(&record.node),
    ) {
        let mut chain =
            std::iter::successors(kept_with.get(&record.node), |kept| kept_with.get(&kept.parent))
                .take(kept_with.len() + 1);
        let taken_along = kept_with.contains_key(&record.node)
            && chain.all(|kept| !revived.contains(&kept.parent))
            && !acted_below(device, &record.node, kept_with);
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

fn positions(trees: &Trees) -> BTreeMap<String, Position> {
    (trees.local().iter())
        .map(|(id, node)| (id.clone(), position(&node.shape)))
        .collect()
}

fn acted_below(
    device: &Device,
    id: &str,
    kept_with: &BTreeMap<String, koidrive_engine::tree::Shape>,
) -> bool {
    let trees = &device.trees;
    let reaches = |entry: &str, parent: &dyn Fn(&str) -> Option<String>| {
        let mut current = Some(entry.to_owned());
        for _ in 0..=trees.local().len() + device.removed_from.len() {
            match current {
                Some(at) if at == id => return true,
                Some(at) => current = parent(&at),
                None => return false,
            }
        }
        false
    };
    let in_local = |at: &str| trees.local().get(at).map(|node| node.shape.parent.clone());
    let removed_from = |at: &str| in_local(at).or_else(|| device.removed_from.get(at).cloned());
    let as_agreed = |entry: &str| {
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
        trees.remote().live(entry).is_some_and(holds)
            || acknowledged_shape(trees, entry).is_some_and(holds)
    };
    let kept_back = |entry: &str| {
        device.kept_back.contains(entry)
            && trees.local().get(entry).is_none()
            && trees.remote().live(entry).is_some()
    };
    (device.acted.iter())
        .filter(|entry| !as_agreed(entry) && !kept_by_r7(device, entry) && !kept_back(entry))
        .any(|entry| {
            reaches(entry, &in_local) || reaches(entry, &removed_from)
        })
}

fn kept_by_r7(device: &Device, entry: &str) -> bool {
    let trees = &device.trees;
    let server_changed = |node: &str| {
        (acknowledged_shape(trees, node))
            .is_some_and(|agreed| (trees.remote().live(node)).is_some_and(|remote| !remote.same(agreed)))
    };
    let moved_by_the_user = |node: &str| {
        (trees.local().get(node)).is_some_and(|local| {
            !trees.reverted().contains(node)
                && acknowledged_shape(trees, node)
                    .is_none_or(|agreed| !local.shape.same_position(agreed))
        })
    };
    let below_in_r = |node: &str| {
        let mut current = trees.remote().live(node).map(|remote| remote.parent.clone());
        for _ in 0..=trees.remote().len() {
            match current {
                Some(at) if at == entry => return true,
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

fn relocation_stands(trees: &Trees, id: &str, to: &koidrive_engine::tree::Shape) -> bool {
    (trees.local().get(id)).is_some_and(|local| local.shape.same_position(to))
        && !(trees.remote().live(id)).is_some_and(|remote| remote.same_position(to))
}

fn note_agreements(
    device: &Device,
    sent_since: &mut BTreeSet<String>,
    agreed: &mut BTreeMap<String, Content>,
    from_server: &mut BTreeSet<String>,
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
        let agreed_now = (agreed.get(id)).is_none_or(|was| !koidrive_engine::tree::same_content(was, content));
        if agreed_now && !sent_since.contains(id) {
            from_server.insert(id.clone());
        }
        sent_since.remove(id);
        agreed.insert(id.clone(), content.clone());
    }
}

fn note_kept_back(device: &mut Device) {
    let trees = &device.trees;
    (device.relocated).retain(|id, moved| relocation_stands(trees, id, &moved.to));
    let kept: Vec<String> = (device.removed_from.keys())
        .filter(|entry| kept_by_r7(device, entry))
        .cloned()
        .collect();
    device.kept_back.extend(kept);
}

fn note_unacknowledged_taken_along_kept_back(
    device: &mut Device,
    kept_with: &BTreeMap<String, koidrive_engine::tree::Shape>,
) {
    let trees = &device.trees;
    let kept: Vec<String> = (kept_with.keys())
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
    let mut device = Device {
        trees: Trees::new(root.clone()),
        ids: IdGen::new(7),
        inos: 0,
        fresh_dir_names,
        rules: Rules::default(),
        touched: BTreeSet::new(),
        acted: BTreeSet::new(),
        removed_from: BTreeMap::new(),
        kept_back: BTreeSet::new(),
        relocated: BTreeMap::new(),
    };
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
    let mut placed_left_out: BTreeSet<String> = BTreeSet::new();
    let mut from_server: BTreeSet<String> = BTreeSet::new();
    for action in local {
        match action.kind {
            5 | 6 => {
                let asked = request_restore(&space, &mut commits, &mut device.ids, action);
                requested += usize::from(asked);
            }
            _ => act_tracked(
                &mut device,
                action,
                &mut placed_left_out,
                &mut from_server,
            ),
        }
    }
    if let Some(prior) = first_link {
        device.trees.settle(commits.aliases());
        commits.first_link(&mut device.trees, &mut device.ids, NOW as u64, prior);
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
        device.receive(&record.changes, false);
        last_seq = record.seq;
    }

    let me = session("me");
    let mut effects = Effects::default();
    let (mut steps, mut quiet, mut due, mut parked_reverted) = (0, false, false, 0);
    let (mut deletes, mut removals) = (0, 0);
    let (mut asked, mut held_once, mut confirmed) = (0, BTreeSet::new(), BTreeSet::new());
    let mut discarded: BTreeMap<String, koidrive_engine::tree::Shape> = BTreeMap::new();
    let mut kept_with: BTreeMap<String, koidrive_engine::tree::Shape> = BTreeMap::new();
    let mut placed: BTreeMap<String, Position> = BTreeMap::new();
    let mut agreed: BTreeMap<String, Content> = BTreeMap::new();
    let mut sent_since: BTreeSet<String> = BTreeSet::new();
    note_agreements(&device, &mut sent_since, &mut agreed, &mut from_server);
    let mut merged_into: BTreeMap<String, String> = BTreeMap::new();
    let mut revived_once: BTreeSet<String> = BTreeSet::new();
    let mut sent_nodes: BTreeSet<String> = BTreeSet::new();
    let mut pending: Option<Question> = None;
    let mut during = during.iter();
    while !quiet {
        steps += 1;
        prop_assert!(steps <= STEPS, "no convergence within {} steps", STEPS);
        device.trees.settle(commits.aliases());
        prop_assert_eq!(device.trees.check(), Ok(()));
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
        (device.trees, commits, effects) =
            reload(&device.trees, &commits, &effects, &device.rules)?;
        note_kept_back(&mut device);
        note_unacknowledged_taken_along_kept_back(&mut device, &kept_with);
        let revived = revive(&device, &discarded, &kept_with, &held_by(commits.questions()));
        revived_once.extend(revived.iter().cloned());
        release_taken_along(&device.trees, &mut kept_with, &revived_once);
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
            let removed = matches!(
                record.effect,
                Effect::RemoveFile { .. } | Effect::RemoveDir { .. }
            );
            let held = device.trees.remote().get(&record.node).is_some();
            let as_discarded = (device.trees.local().get(&record.node))
                .zip(discarded.get(&record.node))
                .is_some_and(|(local, shown)| local.shape.kind == Kind::File && local.shape.same(shown));
            let users = revived.contains(&record.node)
                && !held
                && !sent_nodes.contains(&record.node)
                && !as_discarded;
            prop_assert!(
                !(removed && users),
                "{} is the user's again after the Discard, but the effects remove it",
                record.node
            );
            placement_check(&device, &record, &kept_with, &placed, &revived, &merged_into)?;
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
            continue;
        }
        let before = (device.trees.clone(), commits.questions().to_vec());
        let built = commits.build(
            &mut device.trees,
            &mut device.ids,
            NOW as u64,
            &history,
            "me",
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
                device.acted.remove(id);
                placed.remove(id);
                revived_once.remove(id);
                sent_nodes.remove(id);
            }
            let mut taken = BTreeMap::new();
            for folder in shown.keys() {
                for child in device.trees.local_children(folder) {
                    if !shown.contains_key(child)
                        && (device.trees.excluded(child) || placed_left_out.contains(child))
                    {
                        server_copy(&device, child, &shown, &from_server, &discarded, &mut taken);
                    }
                }
            }
            device.acted.retain(|id| !taken.contains_key(id));
            kept_with.append(&mut taken);
            discarded.extend(shown);
        }
        let revived = revive(&device, &discarded, &kept_with, &held_by(commits.questions()));
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
                }
                prop_assert!(commits.decide(&question.id, answer));
                if answer == Answer::Discard {
                    pending = Some(question.clone());
                }
                asked += 1;
                continue;
            }
            quiet = (&device.trees, commits.questions()) == (&before.0, &before.1[..])
                && during.len() == 0;
            continue;
        };
        let mut sending = Vec::new();
        let mut positioned = Vec::new();
        for op in &commit.ops {
            let id = match &op.op {
                Some(op::Op::Delete(delete)) => &delete.node_id,
                Some(op::Op::CreateFile(create)) => &create.node_id,
                Some(op::Op::CreateDir(create)) => &create.node_id,
                Some(op::Op::Move(moved)) => &moved.node_id,
                Some(op::Op::UpdateContent(update)) => &update.node_id,
                _ => continue,
            };
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
            if let Some(op::Op::Move(moved)) = &op.op
                && let Some(local) = device.trees.local().get(id).filter(|node| !node.parked)
            {
                let synced = device.trees.synced().get(id);
                let remote = device.trees.remote().get(id);
                sent_move_check(
                    moved,
                    &local.shape,
                    synced,
                    remote,
                    &me.device_id,
                    &merged_into,
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
                merged_into.insert(create.node_id.clone(), outcome.node_id.clone());
                for records in [&mut discarded, &mut kept_with] {
                    if let Some(shape) = records.remove(&create.node_id) {
                        records.insert(outcome.node_id.clone(), shape);
                    }
                    for shape in records
                        .values_mut()
                        .filter(|shape| shape.parent == create.node_id)
                    {
                        shape.parent = outcome.node_id.clone();
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
    let renaming = commits.renaming(&device.trees, NOW as u64);
    let reported =
        koidrive_engine::effects::stuck(&device.trees, &effects, commits.in_flight(), &renaming);
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

fn act_tracked(
    device: &mut Device,
    action: &Action,
    placed_left_out: &mut BTreeSet<String>,
    from_server: &mut BTreeSet<String>,
) {
    let before_action = positions(&device.trees);
    let written_before: BTreeMap<String, koidrive_engine::tree::LocalNode> =
        (device.trees.local().iter())
            .map(|(id, node)| (id.clone(), node.clone()))
            .collect();
    let left_out_before: BTreeSet<String> = (written_before.keys())
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
                    (Some(was), Some(now)) => {
                        koidrive_engine::tree::same_content(was, now)
                    }
                    (was, now) => was.is_none() && now.is_none(),
                }
        });
        if !same {
            from_server.remove(id);
        }
    }
    for (id, at) in positions(&device.trees) {
        let parked = (device.trees.local().get(&id))
            .is_some_and(|node| node.parked);
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
