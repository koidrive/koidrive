// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use koidrive_engine::classify::{
    Class, Origin, PAST_POSITIONS_KEPT, REPLACED_VERSIONS_KEPT, Trees, Untaken, Waiting,
};

use koidrive_engine::commit::{
    Commits, Failure, Halt, Lift, LiftError, MAX_COMMIT_BYTES, MAX_OPS, Reconnect,
};

use koidrive_engine::effects::{BatchLimit, Effect, EffectError, Effects, Location};

use koidrive_engine::ids::IdGen;

use koidrive_engine::ignore::RuleFile;

use koidrive_engine::names::{MAX_NAME_BYTES, nfc, normalize_name};

use koidrive_engine::state::{CommitsState, FinishOrigin, MovedHere, PendingFinish, TreesState};

use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};

use koidrive_proto::v1::{self as proto, CommitResult, OpResult, Welcome, error, op, op_result};

use prost::Message;

const ROOT: &str = "root";

const NOW: u64 = 1_794_268_740_000;

const HISTORY: &str = "history";

fn content(digest: u8) -> Content {
    Content {
        size: 3,
        manifest_digest: vec![digest; 32],
        ..Default::default()
    }
}

fn file(parent: &str, name: &str, digest: u8) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::File,
        content: Some(content(digest)),
    }
}

fn dir(parent: &str, name: &str) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    }
}

fn remote(shape: Shape) -> RemoteNode {
    RemoteNode {
        shape,
        trashed: false,
        content_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 1,
        pending_for: None,
        pending_by: None,
    }
}

/// R's node with its position from the record at `rev`.
fn remote_at(shape: Shape, rev: u64) -> RemoteNode {
    RemoteNode {
        meta_rev: rev,
        meta_edit_rev: rev,
        ..remote(shape)
    }
}

fn trashed(shape: Shape) -> RemoteNode {
    RemoteNode {
        trashed: true,
        trash_group: "2:0".into(),
        ..remote(shape)
    }
}

fn local(shape: Shape, ino: u128) -> LocalNode {
    LocalNode {
        shape,
        identity: FileIdentity {
            dev: 1,
            ino,
            birth_ns: Some(0),
        },
        parked: false,
    }
}

/// A node's L, S and R.
type Row = (
    &'static str,
    Option<Shape>,
    Option<Shape>,
    Option<RemoteNode>,
);

/// Trees from (node, L, S, R) rows.
fn trees(nodes: &[Row]) -> Trees {
    let mut trees = Trees::new(ROOT);
    for (i, (id, l, s, r)) in nodes.iter().enumerate() {
        if let Some(shape) = l {
            trees.set_local((*id).into(), local(shape.clone(), i as u128 + 1));
        }
        trees.set_synced((*id).into(), s.clone());
        if let Some(node) = r {
            trees.set_remote((*id).into(), node.clone());
        }
    }
    trees.settle();
    trees
}

/// The same shape in L, S and R.
fn synced(id: &'static str, shape: Shape) -> Row {
    (
        id,
        Some(shape.clone()),
        Some(shape.clone()),
        Some(remote(shape)),
    )
}

fn build(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> Option<proto::Commit> {
    commits.build(trees, ids, NOW, HISTORY, "me", 7).cloned()
}

fn ops(commit: &proto::Commit) -> Vec<op::Op> {
    commit.ops.iter().map(|op| op.op.clone().unwrap()).collect()
}

fn mv(node: &str, parent: &str, name: &str) -> op::Op {
    op::Op::Move(proto::Move {
        node_id: node.into(),
        new_parent_id: parent.into(),
        new_name: name.into(),
        from: None,
        pending_for: String::new(),
    })
}

fn delete(node: &str) -> op::Op {
    op::Op::Delete(proto::Delete {
        node_id: node.into(),
        requires: None,
    })
}

fn update(node: &str, digest: u8) -> op::Op {
    op::Op::UpdateContent(proto::UpdateContent {
        node_id: node.into(),
        content: Some(content(digest)),
        resend: false,
    })
}

fn resend(node: &str, digest: u8) -> op::Op {
    op::Op::UpdateContent(proto::UpdateContent {
        node_id: node.into(),
        content: Some(content(digest)),
        resend: true,
    })
}

fn outcome(code: op_result::Code) -> OpResult {
    OpResult {
        code: code as i32,
        ..Default::default()
    }
}

fn result(seq: u64, results: Vec<OpResult>) -> CommitResult {
    CommitResult {
        seq,
        results,
        changes: vec![],
        ..Default::default()
    }
}

/// Receives `results` for the commit in flight and acknowledges it with R at `seq`.
fn acknowledge(commits: &mut Commits, trees: &mut Trees, results: Vec<OpResult>) {
    commits.received(result(1, results)).unwrap();
    assert!(commits.acknowledge(trees, 1));
    trees.settle();
}

/// Two files that swapped names.
fn swapped() -> Vec<Row> {
    vec![
        (
            "x",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "y",
            Some(file(ROOT, "a", 2)),
            Some(file(ROOT, "b", 2)),
            Some(remote(file(ROOT, "b", 2))),
        ),
    ]
}

/// Applies the effect that moves the reverted `node` back to its R position.
fn moves_back(trees: &mut Trees, ids: &mut IdGen, node: &str) {
    let mut effects = Effects::default();
    let record = effects
        .plan(trees, None, ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert_eq!(record.node, node);
    effects.done(trees, None, &record.effect_id, None).unwrap();
    trees.settle();
    assert_eq!(
        trees.local().get(node).map(|n| &n.shape),
        trees.remote().live(node)
    );
    assert!(trees.reverted().is_empty());
}

/// Trees with `rows`, synced at seq 1. A commit of this device at slot 1 creates `pad` and is
/// applied at seq 2. The user then deletes `rows` here, and the commit at slot 2 that deletes them
/// is applied at seq 5, which trashes each row. A server restored from a backup taken at seq 2
/// counts only slot 1, so every seq up to 2 is from before the loss. Returns the delete's ops.
fn deleted_at_5_then_lost(rows: &[Row]) -> (Commits, Trees, IdGen, Vec<op::Op>) {
    let mut trees = trees(rows);
    trees.set_local("pad".into(), local(file(ROOT, "pad", 9), 99));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let pad = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        ..remote(file(ROOT, "pad", 9))
    };
    trees.set_remote("pad".into(), pad);
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    for (id, ..) in rows.iter().rev() {
        trees.remove_local(id);
    }
    trees.settle();
    let sent = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    for (id, _, shape, _) in rows {
        let trashed = RemoteNode {
            meta_rev: 5,
            meta_edit_rev: 5,
            trash_group: "5:0".into(),
            ..trashed(shape.clone().unwrap())
        };
        trees.set_remote((*id).into(), trashed);
    }
    let applied = vec![outcome(op_result::Code::Applied); sent.len()];
    commits.received(result(5, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(trees.work(), vec![]);
    assert_eq!(commits.held_through(), 5);
    let welcome = Welcome {
        history_id: HISTORY.into(),
        device_seq: 1,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
    assert_eq!(commits.held_through(), 2);
    (commits, trees, ids, sent)
}

/// File a synced with content 1 at seq 1. A commit of this device at slot 1 creates `pad` and is
/// applied at seq 2, and the commit at slot 2 that sets a's content to 2 is applied at seq 5. A
/// server restored from a backup taken at seq 2 counts only slot 1.
fn edited_at_5_then_lost() -> (Commits, Trees, IdGen) {
    let mut trees = trees(&[synced("a", file(ROOT, "a", 1))]);
    trees.set_local("pad".into(), local(file(ROOT, "pad", 9), 99));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let pad = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        ..remote(file(ROOT, "pad", 9))
    };
    trees.set_remote("pad".into(), pad);
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.set_local("a".into(), local(file(ROOT, "a", 2), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 2)]);
    let edited = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        ..remote(file(ROOT, "a", 2))
    };
    trees.set_remote("a".into(), edited);
    commits
        .received(result(5, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(trees.work(), vec![]);
    let welcome = Welcome {
        history_id: HISTORY.into(),
        device_seq: 1,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
    (commits, trees, ids)
}

/// A file of 4,096 chunks, whose change record takes about 150 KiB.
fn large(parent: &str, name: &str) -> Shape {
    let chunk = proto::ChunkRef {
        hash: vec![7; 32],
        size: 4 << 20,
        encoding: 0,
    };
    Shape {
        content: Some(Content {
            chunks: vec![chunk; 4096],
            chunk_count: 4096,
            ..content(1)
        }),
        ..file(parent, name, 1)
    }
}

fn refusal(code: error::Code, retry_after_ms: u64) -> proto::Error {
    proto::Error {
        code: code as i32,
        retry_after_ms,
        ..Default::default()
    }
}

fn welcome(device_seq: u64) -> Welcome {
    Welcome {
        history_id: HISTORY.into(),
        device_seq,
        ..Default::default()
    }
}

/// A parked file h holds root/x in S while R has it at y. n moves to x with new content; the
/// server applies both ops, but S cannot take n at x yet, so its update waits with content 1.
fn a_move_and_edit_whose_s_update_waits() -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        (
            "n",
            Some(file(ROOT, "x", 1)),
            Some(file(ROOT, "old", 0)),
            Some(remote(file(ROOT, "old", 0))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
    trees.set_remote("n".into(), remote(file(ROOT, "x", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    assert_eq!(
        trees.waiting().get("n").map(|waiting| &waiting.shape),
        Some(&Some(file(ROOT, "x", 1)))
    );
    (trees, commits, ids)
}

/// As in `a_move_and_edit_whose_s_update_waits`, but a commit of this device that creates `pad`
/// is applied at seq 2 first, and the move and edit of n at seq 5. A server restored from a backup
/// taken at seq 2 counts only the first, and a bootstrap lists n back at root/old with content 0
/// of seq 1, as S holds it.
fn a_move_and_edit_whose_s_update_waits_lost_at_5() -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        (
            "n",
            Some(file(ROOT, "old", 0)),
            Some(file(ROOT, "old", 0)),
            Some(remote(file(ROOT, "old", 0))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.set_local("pad".into(), local(file(ROOT, "pad", 7), 99));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let pad = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        ..remote(file(ROOT, "pad", 7))
    };
    trees.set_remote("pad".into(), pad);
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
    let moved = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        meta_rev: 5,
        meta_edit_rev: 5,
        ..remote(file(ROOT, "x", 1))
    };
    trees.set_remote("n".into(), moved);
    let applied = vec![outcome(op_result::Code::Applied); 2];
    commits.received(result(5, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(
        trees.waiting().get("n").map(|waiting| &waiting.shape),
        Some(&Some(file(ROOT, "x", 1)))
    );
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    assert_eq!(commits.held_through(), 2);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), remote(file(ROOT, "old", 0)));
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    (trees, commits, ids)
}

fn n_at_x() -> (Trees, Commits, IdGen) {
    let trees = trees(&[(
        "n",
        Some(file(ROOT, "x", 1)),
        Some(file(ROOT, "x", 1)),
        Some(remote(file(ROOT, "x", 1))),
    )]);
    (trees, Commits::default(), IdGen::new(1))
}

fn move_n_to_y_at(seq: u64, trees: &mut Trees, commits: &mut Commits, ids: &mut IdGen) {
    trees.set_local("n".into(), local(file(ROOT, "y", 1), 1));
    trees.settle();
    let commit = build(commits, trees, ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "y")]);
    trees.set_remote("n".into(), remote_at(file(ROOT, "y", 1), seq));
    commits
        .received(result(seq, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(trees, seq));
    trees.settle();
}

fn lose_with_n_at(
    n: RemoteNode,
    device_seq: u64,
    trees: &mut Trees,
    commits: &mut Commits,
    ids: &mut IdGen,
) -> Vec<op::Op> {
    assert_eq!(
        commits.connected(&welcome(device_seq)),
        Reconnect::AheadOfServer
    );
    let (trashed, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    trees.set_remote("n".into(), n);
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        moved,
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    build(commits, trees, ids).map_or_else(Vec::new, |commit| ops(&commit))
}

fn n_moved_to_z_at_7_after_a_commit_at_5() -> (Trees, Commits, IdGen) {
    let (mut trees, mut commits, mut ids) = n_at_x();
    trees.set_local("pad".into(), local(file(ROOT, "pad", 7), 99));
    trees.settle();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("pad".into(), remote_at(file(ROOT, "pad", 7), 5));
    commits
        .received(result(5, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.set_remote("n".into(), remote_at(file(ROOT, "z", 1), 7));
    trees.set_local("n".into(), local(file(ROOT, "z", 1), 1));
    trees.settle();
    (trees, commits, ids)
}

fn take_remote_move(name: &str, rev: u64, trees: &mut Trees) {
    trees.set_remote("n".into(), remote_at(file(ROOT, name, 1), rev));
    trees.set_local("n".into(), local(file(ROOT, name, 1), 1));
    trees.settle();
}

fn past_revisions_of_n(trees: &Trees) -> Vec<u64> {
    let past = trees.state().past_positions;
    (past.get("n").into_iter().flatten())
        .map(|position| position.rev)
        .collect()
}

fn an_edit_to_the_content_a_lost_commit_replaced() -> (Trees, Commits, IdGen) {
    let edited = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        ..remote(file(ROOT, "n", 3))
    };
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "n", 3)),
        Some(file(ROOT, "n", 3)),
        Some(edited),
    )]);
    assert_eq!(trees.class("n"), Class::InSync);
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 1));
    trees.settle();
    let commits = Commits::default();
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), remote(file(ROOT, "n", 2)));
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    (trees, commits, IdGen::new(1))
}

/// Commits a new file `id`, and acknowledges it at `seq` with R holding the file.
fn commit_file(trees: &mut Trees, commits: &mut Commits, ids: &mut IdGen, id: &str, seq: u64) {
    trees.set_local(id.into(), local(file(ROOT, id, 7), 100 + u128::from(seq)));
    trees.settle();
    let commit = build(commits, trees, ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == id),
        "{:?}",
        ops(&commit)
    );
    let created = RemoteNode {
        content_rev: seq,
        edit_rev: seq,
        meta_rev: seq,
        meta_edit_rev: seq,
        ..remote(file(ROOT, id, 7))
    };
    trees.set_remote(id.into(), created);
    let applied = vec![outcome(op_result::Code::Applied)];
    commits.received(result(seq, applied)).unwrap();
    assert!(commits.acknowledge(trees, seq));
    trees.settle();
}

/// Whether `ops` upload content `digest`, to any node.
fn uploads(ops: &[op::Op], digest: u8) -> bool {
    ops.iter().any(|op| match op {
        op::Op::CreateFile(create) => create.content == Some(content(digest)),
        op::Op::UpdateContent(update) => update.content == Some(content(digest)),
        _ => false,
    })
}

/// As in `a_move_and_edit_whose_s_update_waits`, but the server loses the commit, and the
/// bootstrap lists n in the trash. The update is dropped as for any node.
fn a_move_and_edit_lost_while_r_holds_it_in_the_trash() -> (Trees, Commits, IdGen) {
    let (mut trees, mut commits, ids) = a_move_and_edit_whose_s_update_waits();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), trashed(file(ROOT, "old", 0)));
    trees.forget_lost(
        gone,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert!(!trees.waiting().contains_key("n"));
    (trees, commits, ids)
}

/// Folder h at root/x is trashed remotely, and the user moves and edits its file n to root/X. The
/// server applies both ops, but S cannot take n at X while h holds the name key x, so the update
/// waits.
fn a_file_moved_out_of_a_trashed_folder_whose_update_waits() -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[
        (
            "h",
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "x")),
            Some(trashed(dir(ROOT, "x"))),
        ),
        (
            "n",
            Some(file(ROOT, "X", 1)),
            Some(file("h", "old", 0)),
            Some(trashed(file("h", "old", 0))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "X"), update("n", 1)]);
    trees.set_remote("n".into(), remote(file(ROOT, "X", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    assert_eq!(
        trees.waiting().get("n").map(|waiting| &waiting.shape),
        Some(&Some(file(ROOT, "X", 1)))
    );
    (trees, commits, ids)
}

/// Plans and does every effect, failing none, and returns those planned for `node`. An entry an
/// effect creates or writes gets inode 100.
fn effects_for(trees: &mut Trees, ids: &mut IdGen, node: &str) -> Vec<Effect> {
    let mut effects = Effects::default();
    let mut planned = Vec::new();
    for _ in 0..8 {
        let Some(record) = effects
            .plan(trees, None, ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        if record.node == node {
            planned.push(record.effect.clone());
        }
        let written = trees.local().get(&record.node).is_none()
            || matches!(record.effect, Effect::WriteFile { .. });
        let identity = written.then_some(FileIdentity {
            dev: 1,
            ino: 100,
            birth_ns: Some(0),
        });
        effects
            .done(trees, None, &record.effect_id, identity)
            .unwrap();
        trees.settle();
    }
    planned
}

/// GUN-375. n's edit to 1 is in flight when the user and the other device both write 2, so S
/// takes 2. The user writes 1 again before the commit is acknowledged, and the other device then
/// writes 3, before the acknowledgement with `overtaken` and after it otherwise. S keeps 2, which
/// it took after the commit was built, so the user's 1 is an edit of its own and is kept beside
/// the other device's 3. With `restart`, the commit side is restored from its stored state
/// between the send and the acknowledgement.
fn a_late_acknowledgement_keeps_the_content_s_took_since(restart: bool, overtaken: bool) {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "a", 1)),
        Some(file(ROOT, "a", 0)),
        Some(remote(file(ROOT, "a", 0))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 1)]);
    trees.set_local("n".into(), local(file(ROOT, "a", 2), 1));
    trees.set_remote("n".into(), remote(file(ROOT, "a", 2)));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 2)));
    trees.set_local("n".into(), local(file(ROOT, "a", 1), 1));
    if overtaken {
        trees.set_remote("n".into(), remote(file(ROOT, "a", 3)));
        trees.settle();
    }
    if restart {
        commits = Commits::from_state(commits.state()).unwrap();
        trees.restore(trees.state()).unwrap();
        trees.rules_scanned();
    }
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 2)));
    trees.set_remote("n".into(), remote(file(ROOT, "a", 3)));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(copy)] if copy.content == Some(content(1))),
        "{:?}",
        ops(&commit)
    );
}

/// n's edit to 1 is in flight when the user and the other device both write 2, so S takes 2. The
/// other device then writes 0 again, which is downloaded, so S is back at the content it held
/// when the commit was built. The user writes 1 again before the acknowledgement. S keeps 0,
/// since it changed after the commit was built, and the user's 1 is uploaded. With \`restart\`,
/// the commit side and the trees are restored from their stored state before the acknowledgement.
fn a_late_acknowledgement_keeps_the_content_s_took_back(restart: bool) {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "a", 1)),
        Some(file(ROOT, "a", 0)),
        Some(remote(file(ROOT, "a", 0))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 1)]);
    trees.set_local("n".into(), local(file(ROOT, "a", 2), 1));
    trees.set_remote("n".into(), remote(file(ROOT, "a", 2)));
    trees.settle();
    trees.set_remote("n".into(), remote(file(ROOT, "a", 0)));
    trees.settle();
    let download = effects_for(&mut trees, &mut ids, "n");
    assert!(
        matches!(&download[..], [Effect::WriteFile { content: written, .. }] if *written == content(0)),
        "{download:?}"
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 0)));
    trees.set_local("n".into(), local(file(ROOT, "a", 1), 100));
    if restart {
        commits = Commits::from_state(commits.state()).unwrap();
        trees.restore(trees.state()).unwrap();
        trees.rules_scanned();
    }
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 0)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 1)]);
}

/// GUN-378. n's move to b is in flight when the user and the other device both move it to c, so S
/// takes c. The user moves n back to b, and the commit is acknowledged at seq 2 while R already
/// holds the other device's move to c at seq 3, as after a log replay. S keeps c, the position the
/// server took after the commit. When the other device then moves n to d, the user's move back to b
/// is a local move of its own: the disk keeps n at b and the move is sent, instead of n being moved
/// to d. With `restart`, the commit side and the trees are restored from their stored state before
/// the acknowledgement. With `renamed`, the server gave the move another name: n is not reverted
/// to it, since S keeps the newer position.
fn a_late_acknowledgement_keeps_the_position_s_took_after_the_commit(restart: bool, renamed: bool) {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 3));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 1)));
    trees.set_local("n".into(), local(file(ROOT, "b", 1), 1));
    if restart {
        commits = Commits::from_state(commits.state()).unwrap();
        trees.restore(trees.state()).unwrap();
        trees.rules_scanned();
    }
    let applied = match renamed {
        true => OpResult {
            name: "b 2".into(),
            ..outcome(op_result::Code::AppliedRenamed)
        },
        false => outcome(op_result::Code::Applied),
    };
    commits.received(result(2, vec![applied])).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 1)));
    assert!(!trees.reverted().contains("n"));
    trees.set_remote("n".into(), remote_at(file(ROOT, "d", 1), 4));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "b", 1)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
}

/// n's move to b is in flight when a log replay brings the commit at seq 2 and the other device's
/// move back to a, through each of `stops` first, settling after each record but the commit's. S still holds
/// the a it held when the commit was built, and L never took a again, so S takes the b that was
/// sent. The other device's later move then reaches the disk, and the move to b is not sent again.
fn a_late_acknowledgement_takes_the_sent_position_when_only_r_returned(stops: &[&str]) {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 2));
    for (rev, name) in (3..).zip(stops.iter().chain(&["a"])) {
        trees.set_remote("n".into(), remote_at(file(ROOT, name, 1), rev));
        trees.settle();
    }
    let last = 3 + stops.len() as u64;
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, last));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "b", 1)));
    let effects = effects_for(&mut trees, &mut ids, "n");
    assert!(matches!(&effects[..], [Effect::Move { .. }]), "{effects:?}");
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "a", 1)));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

/// GUN-450. n is created at b, and the commit is in flight when the user and the other device both
/// move n to c. A log replay brings the commit at seq 2 and the other device's move at seq 3, so S
/// takes c, the position L and R agree on. The user moves n back to b, and the commit is
/// acknowledged. S keeps c, so when the other device then moves n to d, the user's move back to b
/// is sent instead of n being moved to d. With `settled`, the trees settle after the commit's
/// record as well. With `restart`, the commit side and the trees are restored from their stored
/// state before the acknowledgement. With `renamed`, the server created n as "b 2".
fn a_late_create_acknowledgement_keeps_the_position_s_took_after_the_commit(
    kind: Kind,
    settled: bool,
    restart: bool,
    renamed: bool,
) {
    let at = |name: &str| match kind {
        Kind::File => file(ROOT, name, 1),
        Kind::Dir => dir(ROOT, name),
    };
    let mut trees = trees(&[("n", Some(at("b")), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n")
            || matches!(&ops(&commit)[..], [op::Op::CreateDir(c)] if c.node_id == "n"),
        "{:?}",
        ops(&commit)
    );
    let created = if renamed { "b 2" } else { "b" };
    trees.set_remote("n".into(), remote_at(at(created), 2));
    if settled {
        trees.settle();
        assert_eq!(trees.synced().live("n"), Some(&at("b")));
    }
    trees.set_local("n".into(), local(at("c"), 1));
    trees.set_remote("n".into(), remote_at(at("c"), 3));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&at("c")));
    trees.set_local("n".into(), local(at("b"), 1));
    if restart {
        commits = Commits::from_state(commits.state()).unwrap();
        trees.restore(trees.state()).unwrap();
        trees.rules_scanned();
    }
    let applied = match renamed {
        true => OpResult {
            name: created.into(),
            ..outcome(op_result::Code::AppliedRenamed)
        },
        false => outcome(op_result::Code::Applied),
    };
    commits.received(result(2, vec![applied])).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&at("c")));
    assert!(!trees.reverted().contains("n"));
    trees.set_remote("n".into(), remote_at(at("d"), 4));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live("n"), Some(&at("b")));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
}

/// GUN-453. n's `op` is in flight when the user moves n to c, and the other device moves n to c
/// and edits it to content 2 at seq 3, so L and R agree on the position but not on the content.
/// With `later`, the replay also holds the other device's later move to x at seq 4. After the
/// acknowledgement the effects and the commits take turns: the disk ends with R's content at R's
/// position, and the user's move to c, which the server already holds, is never sent again.
fn a_late_acknowledgement_converges_on_a_remote_edit_made_at_the_position_l_took(
    created: bool,
    later: bool,
) {
    let mut trees = match created {
        true => trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]),
        false => trees(&[(
            "n",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        )]),
    };
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if created && c.node_id == "n")
            || (!created && ops(&commit) == [mv("n", ROOT, "b")]),
        "{:?}",
        ops(&commit)
    );
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 2));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    let edited = |name: &str, rev: u64| RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        ..remote_at(file(ROOT, name, 2), rev)
    };
    trees.set_remote("n".into(), edited("c", 3));
    let (at, last) = match later {
        true => ("x", 4),
        false => ("c", 3),
    };
    if later {
        trees.set_remote("n".into(), edited("x", 4));
    }
    trees.settle();
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, last));
    trees.settle();
    for _ in 0..3 {
        effects_for(&mut trees, &mut ids, "n");
        let commit = build(&mut commits, &mut trees, &mut ids);
        assert_eq!(commit.as_ref().map(ops), None);
    }
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, at, 2)));
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, at, 2)));
}

fn owing_its_name(rows: &[Row]) -> (Commits, Trees, IdGen) {
    marked_by(rows, "me")
}

fn downloaded_at_5() -> (Commits, Trees, IdGen) {
    let mut trees = trees(&[synced("a", file(ROOT, "a", 1))]);
    trees.set_remote("a".into(), version(remote(file(ROOT, "a", 1)), "v1"));
    let edited = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        ..version(remote(file(ROOT, "a", 2)), "v2")
    };
    trees.set_remote("a".into(), edited);
    trees.set_local("a".into(), local(file(ROOT, "a", 2), 1));
    trees.settle();
    assert_eq!(trees.class("a"), Class::InSync);
    (Commits::default(), trees, IdGen::new(1))
}

fn downloaded_then_deleted() -> (Commits, Trees, IdGen) {
    let (commits, mut trees, ids) = downloaded_at_5();
    let deleted = RemoteNode {
        trashed: true,
        trash_group: "6:0".into(),
        meta_rev: 6,
        content_rev: 5,
        edit_rev: 5,
        ..version(remote(file(ROOT, "a", 2)), "v2")
    };
    trees.set_remote("a".into(), deleted);
    trees.settle();
    assert_eq!(trees.class("a"), Class::Remote);
    (commits, trees, ids)
}

fn version(node: RemoteNode, version_id: &str) -> RemoteNode {
    RemoteNode {
        version_id: version_id.into(),
        ..node
    }
}

fn list_after_a_loss(trees: &mut Trees, commits: &Commits, restored: RemoteNode) {
    let synced = trees.taken_from_others(|_| false);
    trees.set_listed_remote("a".into(), restored);
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        synced,
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
}

fn answered_conflict(answer: Answer) -> (Commits, Trees, IdGen, Shape) {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "f", 2)),
        Some(file(ROOT, "f", 1)),
        Some(remote(file(ROOT, "f", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let copy = file(ROOT, "f (conflict)", 2);
    let original = RemoteNode {
        content_rev: 4,
        ..remote_at(file(ROOT, "f", 3), 3)
    };
    trees.set_remote("f".into(), original);
    trees.set_remote("y".into(), remote_at(copy.clone(), 5));
    let made = OpResult {
        node_id: "y".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    acknowledge(&mut commits, &mut trees, vec![made]);
    for (id, shape, ino) in [("f", file(ROOT, "f", 3), 1), ("y", copy.clone(), 2)] {
        trees.set_local(id.into(), local(shape.clone(), ino));
        trees.set_synced(id.into(), Some(shape));
    }
    trees.settle();
    let question = commits.questions()[0].id.clone();
    assert!(commits.decide(&question, answer));
    (commits, trees, ids, copy)
}

fn delete_copy_requiring(content_rev: u64, meta_rev: u64) -> op::Op {
    op::Op::Delete(proto::Delete {
        node_id: "y".into(),
        requires: Some(proto::NodeRevisions {
            node_id: "f".into(),
            content_rev,
            meta_rev,
        }),
    })
}

fn kept_original_recorded() -> Trees {
    let (mut commits, mut trees, mut ids, copy) = answered_conflict(Answer::KeepOriginal);
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let in_trash = RemoteNode {
        trashed: true,
        trash_group: "1:0".into(),
        ..remote_at(copy, 1)
    };
    trees.set_remote("y".into(), in_trash);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(kept_originals(&trees), [("y".into(), "f".into())]);
    trees
}

fn kept_originals(trees: &Trees) -> Vec<(String, String)> {
    trees.state().kept_originals.into_iter().collect()
}

fn reparent_a_folder_at_its_pending_name(answer_first: bool) {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "a", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        ("n", Some(dir(ROOT, "A")), None, None),
        (
            "x",
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "x")),
            Some(remote(dir(ROOT, "x"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(first.ops.len(), 2);
    trees.set_remote("n".into(), remote_at(dir(ROOT, "A (pending)"), 8));
    trees.set_remote("h".into(), remote_at(file("n", "a", 1), 8));
    let answer = result(8, vec![outcome(op_result::Code::Applied); 2]);
    if answer_first {
        commits.received(answer.clone()).unwrap();
        assert!(commits.acknowledge(&mut trees, 8));
        trees.settle();
    } else {
        assert!(!commits.acknowledge(&mut trees, 8));
    }
    trees.set_remote("n".into(), remote_at(dir("x", "A (pending)"), 9));
    if !answer_first {
        commits.received(answer).unwrap();
    }
    commits.acknowledge(&mut trees, 9);
    trees.settle();
    for _ in 0..4 {
        if let Some(commit) = build(&mut commits, &mut trees, &mut ids) {
            let moved = ops(&commit)
                .into_iter()
                .any(|op| matches!(&op, op::Op::Move(m) if m.node_id == "n"));
            assert!(!moved, "{:?}", ops(&commit));
        }
        effects_for(&mut trees, &mut ids, "n");
    }
    assert_eq!(
        trees
            .local()
            .get("n")
            .map(|n| (n.shape.parent.as_str(), n.shape.name.as_str())),
        Some(("x", "A (pending)"))
    );
}
use koidrive_engine::question::{Answer, RecoveryCause, RecoveryTrigger};

fn owed_mv(node: &str, parent: &str, name: &str, from: &str) -> op::Op {
    op::Op::Move(proto::Move {
        node_id: node.into(),
        new_parent_id: parent.into(),
        new_name: name.into(),
        pending_for: String::new(),
        from: Some(proto::Place {
            parent_id: parent.into(),
            name: from.into(),
        }),
    })
}

fn an_edit_in_flight_of_a_file_another_device_moved() -> (Commits, Trees, IdGen) {
    let mut trees = trees(&[synced("a", file(ROOT, "a", 1))]);
    trees.set_remote("a".into(), version(remote(file(ROOT, "a", 1)), "v1"));
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "pad", 2);
    let moved = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        meta_rev: 3,
        ..version(remote(file(ROOT, "b", 2)), "v2")
    };
    trees.set_remote("a".into(), moved);
    trees.set_local("a".into(), local(file(ROOT, "b", 2), 1));
    trees.settle();
    assert_eq!(trees.class("a"), Class::InSync);
    trees.set_local("a".into(), local(file(ROOT, "b", 3), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 3)]);
    (commits, trees, ids)
}

fn list_a_at_its_old_place_after_a_loss(trees: &mut Trees, commits: &Commits) {
    let (trashed, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records();
    trees.set_listed_remote("a".into(), version(remote(file(ROOT, "a", 1)), "v1"));
    trees.remove_remote("pad");
    trees.forget_lost(
        trashed,
        edited,
        synced,
        moved,
        untaken,
        records,
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(trees.synced().live("a"), Some(&file(ROOT, "b", 1)));
}

fn after_an_edit_dropped_in_flight(undo: bool, restored: RemoteNode) -> (Class, Vec<Effect>, Vec<op::Op>) {
    let mut trees = trees(&[synced("a", file(ROOT, "a", 1))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "pad", 2);
    trees.set_local("a".into(), local(file(ROOT, "a", 3), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 3)]);
    let in_flight = commits.edited_in_flight();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    trees.record_dropped_edits(in_flight);
    if undo {
        trees.set_local("a".into(), local(file(ROOT, "a", 1), 1));
        trees.settle();
    }
    let (trashed, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records();
    trees.set_listed_remote("a".into(), restored);
    trees.remove_remote("pad");
    trees.forget_lost(
        trashed,
        edited,
        synced,
        moved,
        untaken,
        records,
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let class = trees.class("a");
    let effects = effects_for(&mut trees, &mut ids, "a");
    let built = build(&mut commits, &mut trees, &mut ids).map(|c| ops(&c)).unwrap_or_default();
    (class, effects, built)
}

fn deleted_after_the_restore() -> RemoteNode {
    RemoteNode {
        meta_rev: 2,
        ..trashed(file(ROOT, "a", 1))
    }
}

fn edited_after_the_restore() -> RemoteNode {
    RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        ..remote(file(ROOT, "a", 4))
    }
}

fn marked_by(rows: &[Row], device: &str) -> (Commits, Trees, IdGen) {
    let mut trees = trees(rows);
    let mut node = trees.remote().get("a").expect("a node of R").clone();
    node.pending_for = Some("a".into());
    node.pending_by = Some(device.into());
    trees.set_remote("a".into(), node);
    trees.settle();
    (Commits::default(), trees, IdGen::new(1))
}

fn owing_a_rename_from_before_marks(trees: &mut Trees) {
    let mut state = trees.state();
    state.pending_finishes.insert(
        "a".into(),
        PendingFinish {
            parent: ROOT.into(),
            pending: "a (pending)".into(),
            name: "a".into(),
            origin: FinishOrigin::Legacy,
            disk_first: false,
            base: None,
            sending: false,
        },
    );
    trees.restore(state).unwrap();
    trees.rules_scanned();
}
