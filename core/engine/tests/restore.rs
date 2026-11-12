// SPDX-License-Identifier: Apache-2.0

//! Restores a person asks for (docs/design/sync-engine.md sections 2.6, 2.7, 4.2 and 6.3), on
//! hand-built trees and results: the `Restore` op a commit carries for each, what its result does
//! to the trees, and how a restore ends or waits when the commit is refused, lost or halted.

use koidrive_engine::classify::{Class, Trees};
use koidrive_engine::commit::{Commits, Failure, Halt, MAX_OPS, Reconnect};
use koidrive_engine::effects::{BatchLimit, Effect, Effects};
use koidrive_engine::ids::{IdGen, uuid_v7};
use koidrive_engine::restore::{
    Malformed, Outcome, RESOLVED_KEPT, Rejection, Resolved, RestoreIntent,
};
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use koidrive_proto::v1::{self as proto, CommitResult, OpResult, Welcome, error, op, op_result};

const NOW: u64 = 1_794_268_740_000;
const HISTORY: &str = "history";

/// A node, version or restore ID.
fn id(n: u128) -> String {
    uuid_v7(1, n)
}

fn root() -> String {
    id(0)
}

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

fn trashed(shape: Shape) -> RemoteNode {
    RemoteNode {
        trashed: true,
        trash_group: "2:0".into(),
        ..remote(shape)
    }
}

/// A node's L, S and R.
type Row = (String, Option<Shape>, Option<Shape>, Option<RemoteNode>);

/// Trees from (node, L, S, R) rows.
fn trees(nodes: &[Row]) -> Trees {
    let mut trees = Trees::new(root());
    for (ino, (id, l, s, r)) in nodes.iter().enumerate() {
        if let Some(shape) = l {
            let identity = FileIdentity {
                dev: 1,
                ino: ino as u128 + 1,
                birth_ns: Some(0),
            };
            let node = LocalNode {
                shape: shape.clone(),
                identity,
                parked: false,
            };
            trees.set_local(id.clone(), node);
        }
        trees.set_synced(id.clone(), s.clone());
        if let Some(node) = r {
            trees.set_remote(id.clone(), node.clone());
        }
    }
    trees.settle();
    trees
}

/// The same shape in L, S and R.
fn synced(id: String, shape: Shape) -> Row {
    let remote = remote(shape.clone());
    (id, Some(shape.clone()), Some(shape), Some(remote))
}

/// A node only R holds, in the trash.
fn in_trash(id: String, shape: Shape) -> Row {
    (id, None, None, Some(trashed(shape)))
}

/// A file only L holds: created here and not sent yet.
fn created(id: String, name: &str) -> Row {
    (id, Some(file(&root(), name, 9)), None, None)
}

fn from_trash(node: &str) -> RestoreIntent {
    RestoreIntent::from_trash(node.into(), None, None).unwrap()
}

fn version(node: &str, version_id: &str) -> RestoreIntent {
    RestoreIntent::version(node.into(), version_id.into()).unwrap()
}

fn build(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> Option<proto::Commit> {
    commits.build(trees, ids, NOW, HISTORY, "me", 7).cloned()
}

fn ops(commit: &proto::Commit) -> Vec<op::Op> {
    commit.ops.iter().map(|op| op.op.clone().unwrap()).collect()
}

fn restore_op(intent: &RestoreIntent) -> op::Op {
    op::Op::Restore(intent.op().clone())
}

fn update(node: &str, digest: u8) -> op::Op {
    op::Op::UpdateContent(proto::UpdateContent {
        node_id: node.into(),
        content: Some(content(digest)),
        resend: false,
    })
}

fn outcome(code: op_result::Code) -> OpResult {
    OpResult {
        code: code as i32,
        ..Default::default()
    }
}

fn rejected(reason: &str) -> OpResult {
    OpResult {
        reject_reason: reason.into(),
        ..outcome(op_result::Code::Rejected)
    }
}

/// Receives `results` for the commit in flight and acknowledges it with R at its seq.
fn acknowledge(commits: &mut Commits, trees: &mut Trees, results: Vec<OpResult>) {
    let result = CommitResult {
        seq: 1,
        results,
        changes: vec![],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(trees, 1));
    trees.settle();
}

fn refuse(commits: &mut Commits, trees: &mut Trees, code: error::Code) -> Failure {
    let error = proto::Error {
        code: code as i32,
        ..Default::default()
    };
    commits.failed(trees, &error).unwrap()
}

fn welcome(device_seq: u64) -> Welcome {
    Welcome {
        history_id: HISTORY.into(),
        device_seq,
        ..Default::default()
    }
}

fn ended(id: &str, intent: &RestoreIntent, outcome: Outcome) -> Resolved {
    Resolved {
        id: id.into(),
        intent: intent.clone(),
        outcome,
    }
}

/// The store's round trip: the state as it is saved, loaded again.
fn reload(commits: &Commits) -> Commits {
    let loaded = Commits::from_state(commits.state()).unwrap();
    assert_eq!(&loaded, commits);
    loaded
}

#[test]
fn an_intent_with_a_malformed_id_cannot_be_built() {
    let (node, other) = (id(1), id(2));
    let bad = || "0190F3A0-1234-7fff-bfff-ffffffffffff".to_string();
    let cases = [
        (RestoreIntent::from_trash(bad(), None, None), "node_id"),
        (
            RestoreIntent::from_trash(String::new(), None, None),
            "node_id",
        ),
        (
            RestoreIntent::from_trash(node.clone(), Some(bad()), None),
            "target_parent_id",
        ),
        (RestoreIntent::version(bad(), other.clone()), "node_id"),
        (RestoreIntent::version(node.clone(), bad()), "version_id"),
        (
            RestoreIntent::version(node.clone(), String::new()),
            "version_id",
        ),
    ];
    for (built, field) in cases {
        assert_eq!(built, Err(Malformed(field)));
    }
    let with_position = |parent: &str, name: &str| {
        RestoreIntent::from_op(proto::Restore {
            node_id: node.clone(),
            version_id: other.clone(),
            target_parent_id: parent.into(),
            name: name.into(),
        })
    };
    assert_eq!(
        with_position(&root(), ""),
        Err(Malformed("target_parent_id"))
    );
    assert_eq!(with_position("", "n"), Err(Malformed("name")));

    let placed = RestoreIntent::from_trash(node.clone(), Some(root()), Some("n".into())).unwrap();
    assert_eq!(
        (
            placed.node(),
            placed.version_id(),
            placed.parent(),
            placed.name()
        ),
        (node.as_str(), None, Some(root().as_str()), Some("n"))
    );
    let old = version(&node, &other);
    assert_eq!(
        (old.version_id(), old.parent(), old.name()),
        (Some(other.as_str()), None, None)
    );
}

#[test]
fn a_trash_restore_is_one_op_and_its_result_is_downloaded() {
    let (n, request) = (id(1), id(100));
    let shape = file(&root(), "f", 1);
    let mut trees = trees(&[in_trash(n.clone(), shape.clone())]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&n);
    assert!(commits.restore(request.clone(), intent.clone()));
    assert_eq!(commits.restores().get(&request), Some(&intent));

    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&intent)]);
    assert_eq!((commit.base_seq, commit.device_seq), (7, 1));
    assert_eq!(
        commits.restores().len(),
        1,
        "a sent restore stays queued until its result is acknowledged"
    );

    trees.set_remote(n.clone(), remote(shape.clone()));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert!(commits.restores().is_empty());
    assert_eq!(
        commits.resolved(),
        [ended(&request, &intent, Outcome::Restored)]
    );
    assert!(commits.problems().is_empty());
    assert_eq!(commits.accepted(), 1);
    assert_eq!(
        (trees.local().get(&n), trees.synced().live(&n)),
        (None, None),
        "neither L nor S takes what the server brought back"
    );
    assert_eq!(trees.class(&n), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);

    let mut effects = Effects::default();
    let record = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap();
    assert_eq!((&record.node, &record.after), (&n, &Some(shape)));
    assert!(matches!(
        &record.effect,
        Effect::WriteFile { replaces: None, .. }
    ));
}

#[test]
fn a_version_restore_leaves_s_and_l_alone_and_the_file_is_replaced() {
    let (n, v, request) = (id(1), id(50), id(100));
    let mut trees = trees(&[synced(n.clone(), file(&root(), "f", 2))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = version(&n, &v);
    commits.restore(request.clone(), intent.clone());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&intent)]);

    trees.set_remote(n.clone(), remote(file(&root(), "f", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(
        commits.resolved(),
        [ended(&request, &intent, Outcome::Restored)]
    );
    let held = |shape: Option<&Shape>| shape.unwrap().content.clone().unwrap();
    assert_eq!(held(trees.synced().live(&n)), content(2));
    assert_eq!(held(trees.local().live(&n)), content(2));
    assert_eq!(trees.class(&n), Class::Remote);

    let mut effects = Effects::default();
    let record = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap();
    let Effect::WriteFile {
        content: written,
        replaces: Some(replaced),
        ..
    } = &record.effect
    else {
        panic!("not a replacing write: {:?}", record.effect);
    };
    assert_eq!((written, &replaced.content), (&content(1), &content(2)));
}

#[test]
fn an_edit_and_a_restore_of_one_file_in_one_commit_leave_the_restored_content_remote() {
    let n = id(1);
    let edited = (
        n.clone(),
        Some(file(&root(), "f", 3)),
        Some(file(&root(), "f", 2)),
        Some(remote(file(&root(), "f", 2))),
    );
    let mut trees = trees(&[edited]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = version(&n, &id(50));
    commits.restore(id(100), intent.clone());
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [update(&n, 3), restore_op(&intent)]);
    let restored = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        ..remote(file(&root(), "f", 4))
    };
    if commits.edits_in_flight(&sent.commit_id, &n) {
        trees.set_remote_edited_here(n.clone(), restored.clone());
    } else {
        trees.set_remote_made_here(n.clone(), restored.clone());
    }
    let result = CommitResult {
        seq: 3,
        results: vec![outcome(op_result::Code::Applied); 2],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.synced().get(&n).unwrap().content, Some(content(3)));
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let trashed = trees.trashed_here();
    let edited = trees.edited_here();
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let moved = trees.moved_here();
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    trees.set_listed_remote(n.clone(), restored);
    trees.forget_lost(
        trashed,
        edited,
        synced,
        moved,
        untaken,
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(next.is_none(), "{:?}", next.as_ref().map(ops));
}

#[test]
fn a_restore_follows_the_edit_of_its_own_file() {
    let (n, v) = (id(1), id(50));
    let edited = (
        n.clone(),
        Some(file(&root(), "f", 3)),
        Some(file(&root(), "f", 2)),
        Some(remote(file(&root(), "f", 2))),
    );
    let intent = version(&n, &v);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commits.restore(id(100), intent.clone());

    let mut together = trees(std::slice::from_ref(&edited));
    let commit = build(&mut commits, &mut together, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update(&n, 3), restore_op(&intent)]);

    // The commit did not fit, so the next ones hold one op: the edit goes first, and the
    // restore waits for the commit after it.
    assert_eq!(
        refuse(&mut commits, &mut together, error::Code::LimitExceeded),
        Failure::Split
    );
    let mut apart = trees(&[edited]);
    let commit = build(&mut commits, &mut apart, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update(&n, 3)]);
    assert_eq!(commits.restores().len(), 1);
    acknowledge(
        &mut commits,
        &mut apart,
        vec![outcome(op_result::Code::Applied)],
    );
    let commit = build(&mut commits, &mut apart, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&intent)]);
}

#[test]
fn a_restore_waits_while_its_node_is_left_out_of_planning() {
    let (n, v) = (id(1), id(50));
    let edited = (
        n.clone(),
        Some(file(&root(), "f", 3)),
        Some(file(&root(), "f", 2)),
        Some(remote(file(&root(), "f", 2))),
    );
    let mut trees = trees(&[edited]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    acknowledge(&mut commits, &mut trees, vec![rejected("LATER_REASON")]);
    assert!(commits.problems().contains(&n));

    // The edit is not planned while the file is a problem. A restore sent now would reach the
    // server before the edit, and the edit would replace the restored version later.
    let intent = version(&n, &v);
    commits.restore(id(100), intent.clone());
    commits.restore(id(101), from_trash(&id(2)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&from_trash(&id(2)))]);
    acknowledge(&mut commits, &mut trees, vec![rejected("NOT_FOUND")]);
    assert_eq!(commits.restores().keys().collect::<Vec<_>>(), [&id(100)]);
    assert_eq!(commits.held_restores().collect::<Vec<_>>(), [&id(100)]);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);

    assert!(commits.clear_problem(&n));
    assert_eq!(commits.held_restores().count(), 0);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update(&n, 3), restore_op(&intent)]);
}

#[test]
fn a_restore_waits_while_s_lags_what_l_and_r_agree_on() {
    // The file was renamed to g and rewritten on the server, and the disk has both. S cannot
    // take the name yet, because it still has the folder that held g, which is parked on disk
    // and on its way to k.
    let (n, m, v) = (id(1), id(2), id(50));
    let downloaded = file(&root(), "g", 1);
    let rows = [
        (
            n.clone(),
            Some(downloaded.clone()),
            Some(file(&root(), "f", 2)),
            Some(remote(downloaded)),
        ),
        (
            m.clone(),
            None,
            Some(dir(&root(), "g")),
            Some(remote(dir(&root(), "k"))),
        ),
    ];
    let mut trees = trees(&rows);
    let folder = |name: &str, parked: bool| LocalNode {
        shape: dir(&root(), name),
        identity: FileIdentity {
            dev: 1,
            ino: 9,
            birth_ns: Some(0),
        },
        parked,
    };
    trees.set_local(m.clone(), folder("g", true));
    trees.settle();
    assert_eq!(trees.synced().live(&n), Some(&file(&root(), "f", 1)));

    // A restore sent now would leave L with content S does not show, and the engine would
    // upload it over the restored version as if the user had written it.
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = version(&n, &v);
    commits.restore(id(100), intent.clone());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(commits.restores().len(), 1);

    trees.set_local(m, folder("k", false));
    trees.settle();
    assert_eq!(trees.synced().live(&n), trees.remote().live(&n));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&intent)]);
}

#[test]
fn a_restore_follows_the_delete_of_its_own_node() {
    let n = id(1);
    let shape = file(&root(), "f", 1);
    let deleted = (n.clone(), None, Some(shape.clone()), Some(remote(shape)));
    let mut trees = trees(&[deleted]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&n);
    commits.restore(id(100), intent.clone());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let delete = op::Op::Delete(proto::Delete {
        node_id: n,
        requires: None,
    });
    assert_eq!(ops(&commit), [delete, restore_op(&intent)]);
}

#[test]
fn a_restore_is_not_held_back_by_other_nodes() {
    let n = id(1);
    let rows = [
        created(id(2), "a"),
        created(id(3), "b"),
        created(id(4), "c"),
        in_trash(n.clone(), file(&root(), "f", 1)),
    ];
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&n);
    commits.restore(id(100), intent.clone());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), 4);
    assert_eq!(ops(&commit)[0], restore_op(&intent));

    assert_eq!(
        refuse(&mut commits, &mut trees, error::Code::LimitExceeded),
        Failure::Split
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), 2, "half the commit that did not fit");
    assert_eq!(ops(&commit)[0], restore_op(&intent));
}

#[test]
fn a_folder_is_not_deleted_in_the_commit_that_restores_a_node_into_it() {
    let (d, n) = (id(1), id(2));
    let folder = dir(&root(), "d");
    let rows = [
        (d.clone(), None, Some(folder.clone()), Some(remote(folder))),
        in_trash(n.clone(), file(&d, "f", 1)),
    ];
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&n);
    commits.restore(id(100), intent.clone());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [restore_op(&intent)],
        "deleting the folder after the restore would trash the restored file again"
    );
}

#[test]
fn a_rejected_restore_is_resolved_and_marks_no_problem() {
    let reasons = [
        ("NOT_FOUND", Rejection::NotFound),
        ("NOT_FILE", Rejection::NotFile),
        ("INVALID_NAME", Rejection::InvalidName),
        ("PARENT_NOT_DIR", Rejection::ParentNotDir),
        ("CYCLE", Rejection::Cycle),
        ("ROOT", Rejection::Root),
        ("LATER_REASON", Rejection::Other("LATER_REASON".into())),
    ];
    for (reason, rejection) in reasons {
        let (n, new, request) = (id(1), id(2), id(100));
        let mut trees = trees(&[created(new.clone(), "a")]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let intent = from_trash(&n);
        commits.restore(request.clone(), intent.clone());
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(commit.ops.len(), 2);

        let results = vec![rejected(reason), outcome(op_result::Code::Applied)];
        acknowledge(&mut commits, &mut trees, results);
        let outcome = Outcome::Rejected(rejection);
        assert_eq!(
            commits.resolved(),
            [ended(&request, &intent, outcome)],
            "{reason}"
        );
        assert!(commits.restores().is_empty(), "{reason}");
        assert!(commits.problems().is_empty(), "{reason}");
        assert!(
            trees.synced().live(&new).is_some(),
            "the other op of the commit is acknowledged"
        );
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None, "{reason}");
    }
}

#[test]
fn a_result_code_no_restore_should_get_still_ends_the_restore() {
    let (n, request) = (id(1), id(100));
    let mut trees = trees(&[]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&n);
    commits.restore(request.clone(), intent.clone());
    build(&mut commits, &mut trees, &mut ids).unwrap();
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Superseded)],
    );
    let code = op_result::Code::Superseded as i32;
    let outcome = Outcome::Rejected(Rejection::Other(format!("result code {code}")));
    assert_eq!(commits.resolved(), [ended(&request, &intent, outcome)]);
    assert!(commits.restores().is_empty());
}

#[test]
fn a_renamed_restore_reports_the_servers_name() {
    let (n, request) = (id(1), id(100));
    let mut trees = trees(&[in_trash(n.clone(), file(&root(), "f", 1))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&n);
    commits.restore(request.clone(), intent.clone());
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote(n.clone(), remote(file(&root(), "f (conflict)", 1)));
    let renamed = OpResult {
        name: "f (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    let outcome = Outcome::Renamed {
        name: "f (conflict)".into(),
    };
    assert_eq!(commits.resolved(), [ended(&request, &intent, outcome)]);
    assert!(
        !trees.reverted().contains(&n),
        "the node is downloaded under the server's name, not moved back"
    );
}

#[test]
fn a_version_restored_as_a_conflict_copy_rekeys_nothing() {
    let (n, v, copy, request) = (id(1), id(50), id(60), id(100));
    let mut trees = trees(&[synced(n.clone(), file(&root(), "f", 2))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = version(&n, &v);
    commits.restore(request.clone(), intent.clone());
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote(copy.clone(), remote(file(&root(), "f (conflict)", 1)));
    let copied = OpResult {
        node_id: copy.clone(),
        name: "f (conflict)".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    acknowledge(&mut commits, &mut trees, vec![copied]);
    let outcome = Outcome::Copied {
        node: copy.clone(),
        name: "f (conflict)".into(),
    };
    assert_eq!(commits.resolved(), [ended(&request, &intent, outcome)]);
    assert_eq!(
        trees.local().live(&n),
        Some(&file(&root(), "f", 2)),
        "the local file holds the node's content, not the version's"
    );
    assert_eq!(trees.synced().live(&n), Some(&file(&root(), "f", 2)));
    assert_eq!(
        (trees.local().get(&copy), trees.synced().live(&copy)),
        (None, None)
    );
    assert_eq!(trees.class(&copy), Class::Remote);
}

#[test]
fn a_restore_that_does_not_fit_alone_is_resolved() {
    let (n, request) = (id(1), id(100));
    let mut trees = trees(&[in_trash(n.clone(), dir(&root(), "d"))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&n);
    commits.restore(request.clone(), intent.clone());
    build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        refuse(&mut commits, &mut trees, error::Code::LimitExceeded),
        Failure::Split
    );
    assert_eq!(
        commits.resolved(),
        [ended(&request, &intent, Outcome::DoesNotFit)]
    );
    assert!(commits.restores().is_empty());
    assert!(
        commits.problems().is_empty(),
        "the node may not exist on this device"
    );
    assert_eq!(commits.in_flight(), None);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_restore_in_a_commit_that_does_not_fit_is_sent_again_in_a_smaller_one() {
    let (n, request) = (id(1), id(100));
    let mut trees = trees(&[created(id(2), "a"), in_trash(n.clone(), dir(&root(), "d"))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&n);
    commits.restore(request, intent.clone());
    assert_eq!(
        build(&mut commits, &mut trees, &mut ids).unwrap().ops.len(),
        2
    );
    assert_eq!(
        refuse(&mut commits, &mut trees, error::Code::LimitExceeded),
        Failure::Split
    );
    assert_eq!(commits.resolved(), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&intent)]);
}

#[test]
fn an_abandoned_commit_sends_its_restores_again() {
    for abandon in [
        |commits: &mut Commits, trees: &mut Trees| {
            refuse(commits, trees, error::Code::HeadMoved) == Failure::Abandoned
        },
        |commits: &mut Commits, trees: &mut Trees| {
            refuse(commits, trees, error::Code::AheadOfServer) == Failure::Resync
        },
        |commits: &mut Commits, _: &mut Trees| commits.abandon(),
    ] {
        let n = id(1);
        let mut trees = trees(&[in_trash(n.clone(), file(&root(), "f", 1))]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let intent = from_trash(&n);
        commits.restore(id(100), intent.clone());
        let first = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert!(abandon(&mut commits, &mut trees));
        assert_eq!(commits.in_flight(), None);
        assert_eq!(commits.restores().len(), 1);
        let second = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(ops(&second), [restore_op(&intent)]);
        assert_ne!(first.commit_id, second.commit_id);
        assert_eq!(second.device_seq, 1, "the slot was never taken");
    }
}

#[test]
fn a_lost_response_resends_the_same_restore_once() {
    let (n, request) = (id(1), id(100));
    let shape = file(&root(), "f", 1);
    let mut trees = trees(&[in_trash(n.clone(), shape.clone())]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&n);
    commits.restore(request.clone(), intent.clone());
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();

    // The process stops before the response arrives. The server counts the commit (w = a + 1).
    let mut commits = reload(&commits);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::Resend);
    assert_eq!(
        commits.in_flight().map(|flight| &flight.commit),
        Some(&sent)
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);

    trees.set_remote(n, remote(shape));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(
        commits.resolved(),
        [ended(&request, &intent, Outcome::Restored)]
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    let mut commits = reload(&commits);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::Idle);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

/// The named scenario of section 7.5, as far as the engine goes before recovery itself exists: a
/// commit with a `Restore` op has an unknown outcome when the history changes. The commit and the
/// restore are kept as the evidence recovery needs, and nothing sends the restore again.
#[test]
fn restore_intent_in_flight() {
    let (n, v) = (id(1), id(50));
    for intent in [version(&n, &v), from_trash(&n)] {
        let mut trees = trees(&[synced(n.clone(), file(&root(), "f", 2))]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let (sent, waiting) = (id(100), id(101));
        commits.restore(sent.clone(), intent.clone());
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        commits.restore(waiting.clone(), from_trash(&id(2)));

        let mut commits = reload(&commits);
        let rewound = Welcome {
            history_id: "another history".into(),
            ..welcome(0)
        };
        assert_eq!(commits.connected(&rewound), Reconnect::Recovery);
        for _ in 0..2 {
            assert!(matches!(commits.halted(), Some(Halt::Recovery(_))));
            assert_eq!(
                commits.in_flight().map(|flight| &flight.commit),
                Some(&commit)
            );
            assert_eq!(commits.restores().get(&sent), Some(&intent));
            assert!(commits.restores().contains_key(&waiting));
            assert_eq!(commits.resolved(), []);
            assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
            commits = reload(&commits);
            assert_eq!(commits.connected(&rewound), Reconnect::Recovery);
            assert_eq!(commits.connected(&welcome(1)), Reconnect::Recovery);
        }
    }
}

#[test]
fn a_policy_halt_keeps_queued_restores() {
    let n = id(1);
    let mut trees = trees(&[]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commits.restore(id(100), from_trash(&n));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let code = error::Code::InvalidArgument as i32;
    assert_eq!(
        refuse(&mut commits, &mut trees, error::Code::InvalidArgument),
        Failure::Halted(Halt::Policy { code })
    );
    assert!(commits.restore(id(101), from_trash(&id(2))));
    let mut commits = reload(&commits);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Idle);
    assert_eq!(commits.restores().len(), 2);
    assert_eq!(commits.resolved(), []);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_read_only_device_rejects_the_restores_it_cannot_send() {
    let read_only = Welcome {
        read_only: true,
        ..welcome(0)
    };
    let refused = Outcome::Rejected(Rejection::ReadOnly);
    let (first, second) = (from_trash(&id(1)), from_trash(&id(2)));

    // Queued, then the Welcome says read-only. A later request ends at once.
    let mut commits = Commits::default();
    commits.restore(id(100), first.clone());
    assert_eq!(commits.connected(&read_only), Reconnect::Idle);
    assert_eq!(
        commits.resolved(),
        [ended(&id(100), &first, refused.clone())]
    );
    assert!(commits.restore(id(101), second.clone()));
    assert!(commits.restores().is_empty());
    assert_eq!(
        commits.resolved()[1],
        ended(&id(101), &second, refused.clone())
    );

    // The server refuses the commit as read-only: nothing of it was applied.
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let mut trees = trees(&[]);
    commits.restore(id(100), first.clone());
    build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        refuse(&mut commits, &mut trees, error::Code::ReadOnly),
        Failure::Halted(Halt::ReadOnly)
    );
    assert_eq!(
        commits.resolved(),
        [ended(&id(100), &first, refused.clone())]
    );

    // A commit whose response was lost may have been applied, so its restore waits for the
    // result. One that was never sent ends.
    let mut commits = Commits::default();
    commits.restore(id(100), first.clone());
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    commits.restore(id(101), second.clone());
    let read_only = Welcome {
        device_seq: 1,
        ..read_only
    };
    assert_eq!(commits.connected(&read_only), Reconnect::Idle);
    assert_eq!(commits.resolved(), [ended(&id(101), &second, refused)]);
    assert_eq!(commits.restores().get(&id(100)), Some(&first));
    let mut commits = reload(&commits);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::Resend);
    assert_eq!(
        commits.in_flight().map(|flight| &flight.commit),
        Some(&sent)
    );
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(
        commits.resolved()[1],
        ended(&id(100), &first, Outcome::Restored)
    );
}

#[test]
fn a_request_repeated_under_its_id_is_queued_once() {
    let (n, request) = (id(1), id(100));
    let mut trees = trees(&[]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert!(commits.restore(request.clone(), from_trash(&n)));
    assert!(!commits.restore(request.clone(), from_trash(&n)));
    assert!(!commits.restore(request.clone(), from_trash(&id(2))));
    assert_eq!(commits.restores().len(), 1);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&from_trash(&n))]);
    acknowledge(&mut commits, &mut trees, vec![rejected("NOT_FOUND")]);
    assert!(
        !commits.restore(request, from_trash(&n)),
        "a restore that ended is not sent again under its ID"
    );
    assert!(commits.restores().is_empty());
}

#[test]
fn two_requests_for_the_same_restore_are_sent_and_resolved_one_each() {
    let n = id(1);
    let mut trees = trees(&[]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&n);
    commits.restore(id(101), intent.clone());
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(first.ops.len(), 1);
    // An equal request with an earlier ID arrives while the first is in flight.
    commits.restore(id(100), intent.clone());
    let mut commits = reload(&commits);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(commits.resolved().len(), 1);
    assert_eq!(commits.restores().len(), 1);
    let second = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&second), [restore_op(&intent)]);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    let ended: Vec<&String> = commits.resolved().iter().map(|ended| &ended.id).collect();
    assert_eq!(ended, [&id(100), &id(101)]);
}

#[test]
fn a_thousand_restores_fill_a_commit_and_the_last_outcomes_are_kept() {
    let extra = 5;
    let mut trees = trees(&[]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    for n in 0..(MAX_OPS + extra) as u128 {
        commits.restore(id(10_000 + n), from_trash(&id(1 + n)));
    }
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), MAX_OPS);
    assert_eq!(
        ops(&commit)[0],
        restore_op(&from_trash(&id(1))),
        "in request order"
    );
    acknowledge(
        &mut commits,
        &mut trees,
        vec![rejected("NOT_FOUND"); MAX_OPS],
    );
    assert_eq!(commits.restores().len(), extra);

    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), extra);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); extra],
    );
    assert!(commits.restores().is_empty());
    let resolved = commits.resolved();
    assert_eq!(resolved.len(), RESOLVED_KEPT);
    assert_eq!(
        resolved[0].id,
        id(10_000 + extra as u128),
        "the oldest are dropped"
    );
    assert_eq!(resolved[RESOLVED_KEPT - 1].outcome, Outcome::Restored);
    reload(&commits);
}

#[test]
fn an_edit_sent_with_a_restore_from_the_trash_is_an_edit_of_this_device() {
    let n = id(1);
    let edited = (
        n.clone(),
        Some(file(&root(), "f", 3)),
        Some(file(&root(), "f", 2)),
        Some(trashed(file(&root(), "f", 2))),
    );
    let mut trees = trees(&[edited]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commits.restore(id(100), from_trash(&n));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(commits.edits_in_flight(&sent.commit_id, &n));
}

#[test]
fn a_restore_into_a_trashed_folder_is_sent_after_the_folders_own_move() {
    let (p, a, f) = (id(1), id(2), id(3));
    let rows = [
        in_trash(p.clone(), dir(&root(), "p")),
        (
            a.clone(),
            Some(dir(&root(), "b")),
            Some(dir(&root(), "a")),
            Some(trashed(dir(&p, "a"))),
        ),
        in_trash(f.clone(), file(&a, "f", 1)),
    ];
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&f);
    commits.restore(id(100), intent.clone());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let moved = op::Op::Move(proto::Move {
        pending_for: String::new(),
        node_id: a.clone(),
        new_parent_id: root(),
        new_name: "b".into(),
        from: None,
    });
    assert_eq!(ops(&commit), [moved, restore_op(&intent)]);
}

#[test]
fn a_trashed_folder_restored_into_itself_or_below_itself_is_sent_for_the_server_to_refuse() {
    let (a, b) = (id(1), id(2));
    for target in [&a, &b] {
        let mut trees = trees(&[
            in_trash(a.clone(), dir(&root(), "a")),
            in_trash(b.clone(), dir(&a, "b")),
        ]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let intent = RestoreIntent::from_trash(a.clone(), Some(target.clone()), None).unwrap();
        commits.restore(id(100), intent.clone());
        let commit = build(&mut commits, &mut trees, &mut ids);
        assert_eq!(
            commit.map(|commit| ops(&commit)),
            Some(vec![restore_op(&intent)]),
            "{target}"
        );
    }
}

#[test]
fn a_move_onto_the_name_of_a_folder_moving_into_a_folder_restored_into_it_goes_through_a_pending_name()
 {
    let (p, a, b, c) = (id(1), id(2), id(3), id(4));
    let rows = [
        in_trash(p.clone(), dir(&root(), "p")),
        (
            a.clone(),
            Some(dir(&root(), "n")),
            Some(dir(&root(), "a")),
            Some(trashed(dir(&p, "a"))),
        ),
        (
            b.clone(),
            Some(dir(&c, "n")),
            Some(dir(&root(), "n")),
            Some(remote(dir(&root(), "n"))),
        ),
        (
            c.clone(),
            Some(dir(&root(), "c")),
            Some(dir(&root(), "c")),
            Some(trashed(dir(&p, "c"))),
        ),
    ];
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = RestoreIntent::from_trash(c.clone(), Some(a.clone()), None).unwrap();
    commits.restore(id(100), intent.clone());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let mv = |node: &str, parent: &str, name: &str, pending_for: &str| {
        op::Op::Move(proto::Move {
            pending_for: pending_for.into(),
            node_id: node.into(),
            new_parent_id: parent.into(),
            new_name: name.into(),
            from: None,
        })
    };
    assert_eq!(
        ops(&commit),
        [
            mv(&a, &root(), "n (pending)", "n"),
            restore_op(&intent),
            mv(&b, &c, "n", ""),
            mv(&a, &root(), "n", ""),
        ]
    );
}

#[test]
fn a_folder_renamed_onto_a_held_name_and_restored_into_a_folder_restored_into_it_is_planned() {
    let (p, x, h, r) = (id(1), id(2), id(3), id(4));
    let rows = [
        in_trash(p.clone(), dir(&root(), "p")),
        (
            x.clone(),
            Some(dir(&root(), "n")),
            Some(dir(&root(), "x")),
            Some(trashed(dir(&p, "x"))),
        ),
        (
            h.clone(),
            Some(dir(&root(), "n")),
            Some(dir(&root(), "n")),
            Some(remote(dir(&root(), "n"))),
        ),
        in_trash(r.clone(), dir(&p, "r")),
    ];
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let into_r = RestoreIntent::from_trash(x.clone(), Some(r.clone()), None).unwrap();
    let into_x = RestoreIntent::from_trash(r.clone(), Some(x.clone()), None).unwrap();
    commits.restore(id(100), into_r.clone());
    commits.restore(id(101), into_x.clone());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let moved = op::Op::Move(proto::Move {
        pending_for: String::new(),
        node_id: x.clone(),
        new_parent_id: root(),
        new_name: "n".into(),
        from: None,
    });
    assert_eq!(
        ops(&commit),
        [restore_op(&into_x), moved, restore_op(&into_r)]
    );
}

#[test]
fn two_trashed_folders_restored_into_each_other_are_both_sent() {
    let (a, b) = (id(1), id(2));
    let mut trees = trees(&[
        in_trash(a.clone(), dir(&root(), "a")),
        in_trash(b.clone(), dir(&root(), "b")),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let into_b = RestoreIntent::from_trash(a.clone(), Some(b.clone()), None).unwrap();
    let into_a = RestoreIntent::from_trash(b, Some(a), None).unwrap();
    commits.restore(id(100), into_b.clone());
    commits.restore(id(101), into_a.clone());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&into_b), restore_op(&into_a)]);
}

#[test]
fn an_explicit_restore_of_a_file_its_user_removed_is_downloaded() {
    let n = id(1);
    let shape = file(&root(), "f", 1);
    let mut trees = trees(&[(
        n.clone(),
        None,
        Some(shape.clone()),
        Some(trashed(shape.clone())),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&n);
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&intent)]);

    let restored = RemoteNode {
        meta_rev: 2,
        ..remote(shape)
    };
    trees.set_remote_made_here(n.clone(), restored);
    let result = CommitResult {
        seq: 2,
        results: vec![outcome(op_result::Code::Applied)],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    assert_eq!(trees.class(&n), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_restore_of_a_folder_its_user_removed_downloads_the_files_in_it() {
    let (d, f) = (id(1), id(2));
    let folder = dir(&root(), "d");
    let inside = file(&d, "f", 1);
    let mut trees = trees(&[
        (
            d.clone(),
            None,
            Some(folder.clone()),
            Some(trashed(folder.clone())),
        ),
        (
            f.clone(),
            None,
            Some(inside.clone()),
            Some(trashed(inside.clone())),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&d);
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&intent)]);

    let at_2 = |shape: Shape| RemoteNode {
        meta_rev: 2,
        ..remote(shape)
    };
    trees.set_remote_made_here(d.clone(), at_2(folder));
    trees.set_remote_made_here(f.clone(), at_2(inside));
    let result = CommitResult {
        seq: 2,
        results: vec![outcome(op_result::Code::Applied)],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    assert_eq!(trees.class(&d), Class::Remote);
    assert_eq!(trees.class(&f), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_restore_the_server_rejects_leaves_the_removal_a_later_move_out_of_the_trash_deletes_again() {
    let (a, g, missing) = (id(1), id(2), id(3));
    let folder = dir(&root(), "a");
    let inside = file(&a, "g", 1);
    let mut trees = trees(&[
        (
            a.clone(),
            Some(folder.clone()),
            Some(folder.clone()),
            Some(trashed(folder)),
        ),
        (
            g.clone(),
            None,
            Some(inside.clone()),
            Some(trashed(inside.clone())),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = RestoreIntent::from_trash(a.clone(), Some(missing), None).unwrap();
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&intent)]);
    acknowledge(&mut commits, &mut trees, vec![rejected("NOT_FOUND")]);

    let renamed = LocalNode {
        shape: dir(&root(), "A"),
        identity: FileIdentity {
            dev: 1,
            ino: 1,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local(a.clone(), renamed);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let moved = op::Op::Move(proto::Move {
        node_id: a.clone(),
        new_parent_id: root(),
        new_name: "A".into(),
        pending_for: String::new(),
        from: None,
    });
    assert_eq!(ops(&commit), [moved]);
    let at_2 = |shape: Shape| RemoteNode {
        meta_rev: 2,
        ..remote(shape)
    };
    trees.set_remote_made_here(a.clone(), at_2(dir(&root(), "A")));
    trees.set_remote_made_here(g.clone(), at_2(inside));
    let result = CommitResult {
        seq: 2,
        results: vec![outcome(op_result::Code::Applied)],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let delete = op::Op::Delete(proto::Delete {
        node_id: g.clone(),
        requires: None,
    });
    assert_eq!(ops(&commit), [delete]);
}

fn renamed_out_of_the_trash(a: &str, g: &str) -> Trees {
    let folder = dir(&root(), "a");
    let inside = file(a, "g", 1);
    trees(&[
        (
            a.to_owned(),
            Some(dir(&root(), "A")),
            Some(folder.clone()),
            Some(trashed(folder)),
        ),
        (
            g.to_owned(),
            None,
            Some(inside.clone()),
            Some(trashed(inside)),
        ),
    ])
}

fn moved(node: &str, parent: &str, name: &str) -> op::Op {
    op::Op::Move(proto::Move {
        node_id: node.into(),
        new_parent_id: parent.into(),
        new_name: name.into(),
        pending_for: String::new(),
        from: None,
    })
}

fn revived(
    commits: &mut Commits,
    trees: &mut Trees,
    nodes: &[(&str, Shape)],
    results: Vec<OpResult>,
) {
    for (node, shape) in nodes {
        let node_at_2 = RemoteNode {
            meta_rev: 2,
            ..remote(shape.clone())
        };
        trees.set_remote_made_here((*node).to_owned(), node_at_2);
    }
    let result = CommitResult {
        seq: 2,
        results,
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(trees, 2));
    trees.settle();
}

#[test]
fn a_rejected_restore_sent_with_a_move_that_brings_its_file_back_leaves_the_file_deleted() {
    let (a, g, missing) = (id(1), id(2), id(3));
    let mut trees = renamed_out_of_the_trash(&a, &g);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = version(&g, &missing);
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&intent), moved(&a, &root(), "A")]);

    let back = [
        (a.as_str(), dir(&root(), "A")),
        (g.as_str(), file(&a, "g", 1)),
    ];
    let results = vec![rejected("NOT_FOUND"), outcome(op_result::Code::Applied)];
    revived(&mut commits, &mut trees, &back, results);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let delete = op::Op::Delete(proto::Delete {
        node_id: g.clone(),
        requires: None,
    });
    assert_eq!(ops(&commit), [delete]);
}

#[test]
fn a_restore_sent_with_a_move_that_already_brought_its_file_back_downloads_the_file() {
    let (a, g) = (id(1), id(2));
    let mut trees = renamed_out_of_the_trash(&a, &g);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&g);
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&intent), moved(&a, &root(), "A")]);

    let back = [
        (a.as_str(), dir(&root(), "A")),
        (g.as_str(), file(&a, "g", 1)),
    ];
    let results = vec![outcome(op_result::Code::Applied); 2];
    revived(&mut commits, &mut trees, &back, results);
    assert_eq!(trees.class(&g), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_restore_of_a_renamed_trashed_folder_with_a_removed_member_is_sent_with_its_rename() {
    let (a, g) = (id(1), id(2));
    let mut trees = renamed_out_of_the_trash(&a, &g);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&a);
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [moved(&a, &root(), "A"), restore_op(&intent)]);

    let back = [
        (a.as_str(), dir(&root(), "A")),
        (g.as_str(), file(&a, "g", 1)),
    ];
    let results = vec![outcome(op_result::Code::Applied); 2];
    revived(&mut commits, &mut trees, &back, results);
    assert_eq!(trees.class(&g), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_restore_of_a_removed_file_under_a_renamed_trashed_folder_is_sent_with_the_rename() {
    let (p, a, b) = (id(1), id(2), id(3));
    let parent = dir(&root(), "p");
    let folder = dir(&p, "a");
    let inside = file(&a, "b", 1);
    let mut trees = trees(&[
        (
            p.clone(),
            Some(parent.clone()),
            Some(parent.clone()),
            Some(trashed(parent)),
        ),
        (
            a.clone(),
            Some(dir(&p, "a2")),
            Some(folder.clone()),
            Some(trashed(folder)),
        ),
        (b.clone(), None, Some(inside.clone()), Some(trashed(inside))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&b);
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [moved(&a, &p, "a2"), restore_op(&intent)]);
}

#[test]
fn a_restore_of_a_removed_file_into_a_renamed_trashed_folder_is_sent_with_the_rename() {
    let (p, a, b, c) = (id(1), id(2), id(3), id(4));
    let parent = dir(&root(), "p");
    let folder = dir(&p, "a");
    let target = dir(&p, "c");
    let inside = file(&a, "b", 1);
    let mut trees = trees(&[
        (
            p.clone(),
            Some(parent.clone()),
            Some(parent.clone()),
            Some(trashed(parent)),
        ),
        (a.clone(), None, Some(folder.clone()), Some(trashed(folder))),
        (
            c.clone(),
            Some(dir(&p, "c2")),
            Some(target.clone()),
            Some(trashed(target)),
        ),
        (b.clone(), None, Some(inside.clone()), Some(trashed(inside))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = RestoreIntent::from_trash(b.clone(), Some(c.clone()), None).unwrap();
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [moved(&c, &p, "c2"), restore_op(&intent)]);
}

#[test]
fn a_restore_of_a_file_in_a_folder_its_user_removed_downloads_the_folder() {
    let (a, g) = (id(1), id(2));
    let folder = dir(&root(), "a");
    let inside = file(&a, "g", 1);
    let mut trees = trees(&[
        (
            a.clone(),
            None,
            Some(folder.clone()),
            Some(trashed(folder.clone())),
        ),
        (
            g.clone(),
            None,
            Some(inside.clone()),
            Some(trashed(inside.clone())),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&g);
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&intent)]);

    let back = [(a.as_str(), folder), (g.as_str(), inside)];
    revived(
        &mut commits,
        &mut trees,
        &back,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.class(&a), Class::Remote);
    assert_eq!(trees.class(&g), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn two_restores_of_one_file_keep_the_folder_the_first_brings_back() {
    let (a, g) = (id(1), id(2));
    let folder = dir(&root(), "a");
    let inside = file(&a, "g", 1);
    let mut trees = trees(&[
        (
            a.clone(),
            None,
            Some(folder.clone()),
            Some(trashed(folder.clone())),
        ),
        (
            g.clone(),
            None,
            Some(inside.clone()),
            Some(trashed(inside.clone())),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let original = from_trash(&g);
    let at_root = RestoreIntent::from_trash(g.clone(), Some(root()), None).unwrap();
    assert!(commits.restore(id(100), original.clone()));
    assert!(commits.restore(id(101), at_root.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [restore_op(&original), restore_op(&at_root)]);

    let back = [(a.as_str(), folder), (g.as_str(), inside)];
    revived(
        &mut commits,
        &mut trees,
        &back,
        vec![outcome(op_result::Code::Applied); 2],
    );
    assert_eq!(trees.class(&a), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_restore_sent_with_the_delete_of_its_file_keeps_the_file_when_another_device_deletes_its_folder_first()
 {
    let (a, g) = (id(1), id(2));
    let folder = dir(&root(), "a");
    let inside = file(&a, "g", 1);
    let mut trees = trees(&[
        (
            a.clone(),
            Some(dir(&root(), "A")),
            Some(folder.clone()),
            Some(remote(folder.clone())),
        ),
        (
            g.clone(),
            None,
            Some(inside.clone()),
            Some(remote(inside.clone())),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&g);
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let delete = op::Op::Delete(proto::Delete {
        node_id: g.clone(),
        requires: None,
    });
    assert_eq!(
        ops(&commit),
        [moved(&a, &root(), "A"), delete, restore_op(&intent)]
    );

    let deleted_elsewhere = |shape: Shape| RemoteNode {
        meta_rev: 2,
        ..trashed(shape)
    };
    trees.set_remote(a.clone(), deleted_elsewhere(folder));
    trees.set_remote(g.clone(), deleted_elsewhere(inside.clone()));
    trees.settle();
    for (node, shape) in [(&a, dir(&root(), "A")), (&g, inside)] {
        let back = RemoteNode {
            meta_rev: 3,
            ..remote(shape)
        };
        trees.set_remote_made_here(node.clone(), back);
    }
    let results = vec![
        outcome(op_result::Code::Applied),
        outcome(op_result::Code::Superseded),
        outcome(op_result::Code::Applied),
    ];
    let result = CommitResult {
        seq: 3,
        results,
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.class(&g), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_restore_of_a_live_folder_leaves_a_file_deleted_below_it_deleted_when_a_move_brings_it_back() {
    let (z, a, y) = (id(1), id(2), id(3));
    let top = dir(&root(), "z");
    let folder = dir(&z, "a");
    let inside = file(&a, "y", 1);
    let mut trees = trees(&[
        (
            z.clone(),
            Some(top.clone()),
            Some(top.clone()),
            Some(remote(top.clone())),
        ),
        (
            a.clone(),
            Some(dir(&z, "A")),
            Some(folder.clone()),
            Some(remote(folder.clone())),
        ),
        (
            y.clone(),
            None,
            Some(inside.clone()),
            Some(remote(inside.clone())),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&z);
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let sent = ops(&commit);
    let deleted_elsewhere = |shape: Shape| RemoteNode {
        meta_rev: 2,
        ..trashed(shape)
    };
    trees.set_remote(a.clone(), deleted_elsewhere(folder));
    trees.set_remote(y.clone(), deleted_elsewhere(inside.clone()));
    trees.settle();
    for (node, shape) in [(&a, dir(&z, "A")), (&y, inside)] {
        let back = RemoteNode {
            meta_rev: 3,
            ..remote(shape)
        };
        trees.set_remote_made_here(node.clone(), back);
    }
    let results = sent
        .iter()
        .map(|op| match op {
            op::Op::Delete(_) => outcome(op_result::Code::Superseded),
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    let result = CommitResult {
        seq: 3,
        results,
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    let next = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    let delete = op::Op::Delete(proto::Delete {
        node_id: y.clone(),
        requires: None,
    });
    assert_eq!(next, Some(vec![delete]));
}

#[test]
fn a_restore_sent_with_the_delete_of_its_folder_keeps_the_files_in_it_when_another_device_deletes_its_parent_first()
 {
    let (a, b, c) = (id(1), id(2), id(3));
    let parent = dir(&root(), "a");
    let folder = dir(&a, "b");
    let inside = file(&b, "c", 1);
    let mut trees = trees(&[
        (
            a.clone(),
            Some(dir(&root(), "A")),
            Some(parent.clone()),
            Some(remote(parent.clone())),
        ),
        (
            b.clone(),
            None,
            Some(folder.clone()),
            Some(remote(folder.clone())),
        ),
        (
            c.clone(),
            None,
            Some(inside.clone()),
            Some(remote(inside.clone())),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = from_trash(&b);
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let sent = ops(&commit);
    assert!(sent.contains(&restore_op(&intent)));
    let deleted_elsewhere = |shape: Shape| RemoteNode {
        meta_rev: 2,
        ..trashed(shape)
    };
    for (node, shape) in [(&a, parent), (&b, folder.clone()), (&c, inside.clone())] {
        trees.set_remote(node.clone(), deleted_elsewhere(shape));
    }
    trees.settle();
    for (node, shape) in [(&a, dir(&root(), "A")), (&b, folder), (&c, inside)] {
        let back = RemoteNode {
            meta_rev: 3,
            ..remote(shape)
        };
        trees.set_remote_made_here(node.clone(), back);
    }
    let results = (sent.iter())
        .map(|op| match op {
            op::Op::Delete(_) => outcome(op_result::Code::Superseded),
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    let result = CommitResult {
        seq: 3,
        results,
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.class(&b), Class::Remote);
    assert_eq!(trees.class(&c), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_restore_of_a_file_its_folders_delete_planned_keeps_the_file_when_another_device_deletes_their_parent_first()
 {
    let (p, a, b, d) = (id(1), id(2), id(3), id(4));
    let parent = dir(&root(), "p");
    let folder = dir(&p, "a");
    let inside = file(&a, "b", 1);
    let target = dir(&p, "d");
    let mut trees = trees(&[
        (
            p.clone(),
            Some(dir(&root(), "P")),
            Some(parent.clone()),
            Some(remote(parent.clone())),
        ),
        (
            a.clone(),
            None,
            Some(folder.clone()),
            Some(remote(folder.clone())),
        ),
        (
            b.clone(),
            None,
            Some(inside.clone()),
            Some(remote(inside.clone())),
        ),
        (
            d.clone(),
            Some(target.clone()),
            Some(target.clone()),
            Some(remote(target.clone())),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = RestoreIntent::from_trash(b.clone(), Some(d.clone()), None).unwrap();
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let delete = op::Op::Delete(proto::Delete {
        node_id: a.clone(),
        requires: None,
    });
    let sent = ops(&commit);
    assert_eq!(sent, [moved(&p, &root(), "P"), delete, restore_op(&intent)]);
    let deleted_elsewhere = |shape: Shape| RemoteNode {
        meta_rev: 2,
        ..trashed(shape)
    };
    let nodes = [
        (&p, parent),
        (&a, folder.clone()),
        (&b, inside.clone()),
        (&d, target.clone()),
    ];
    for (node, shape) in nodes {
        trees.set_remote(node.clone(), deleted_elsewhere(shape));
    }
    trees.settle();
    let nodes = [
        (&p, dir(&root(), "P")),
        (&a, folder),
        (&b, inside),
        (&d, target),
    ];
    for (node, shape) in nodes {
        let back = RemoteNode {
            meta_rev: 3,
            ..remote(shape)
        };
        trees.set_remote_made_here(node.clone(), back);
    }
    let results = (sent.iter())
        .map(|op| match op {
            op::Op::Delete(_) => outcome(op_result::Code::Superseded),
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    let result = CommitResult {
        seq: 3,
        results,
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.class(&b), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_restore_of_a_file_its_folders_delete_planned_leaves_the_other_files_of_that_delete_deleted() {
    let (p, a, b, d, e) = (id(1), id(2), id(3), id(4), id(5));
    let parent = dir(&root(), "p");
    let folder = dir(&p, "a");
    let inside = file(&a, "b", 1);
    let target = dir(&p, "d");
    let sibling = file(&a, "e", 2);
    let mut trees = trees(&[
        (
            p.clone(),
            Some(dir(&root(), "P")),
            Some(parent.clone()),
            Some(remote(parent.clone())),
        ),
        (
            a.clone(),
            None,
            Some(folder.clone()),
            Some(remote(folder.clone())),
        ),
        (
            b.clone(),
            None,
            Some(inside.clone()),
            Some(remote(inside.clone())),
        ),
        (
            d.clone(),
            Some(target.clone()),
            Some(target.clone()),
            Some(remote(target.clone())),
        ),
        (
            e.clone(),
            None,
            Some(sibling.clone()),
            Some(remote(sibling.clone())),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let intent = RestoreIntent::from_trash(b.clone(), Some(d.clone()), None).unwrap();
    assert!(commits.restore(id(100), intent.clone()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let sent = ops(&commit);
    let deleted_elsewhere = |shape: Shape| RemoteNode {
        meta_rev: 2,
        ..trashed(shape)
    };
    let nodes = [
        (&p, parent),
        (&a, folder.clone()),
        (&b, inside.clone()),
        (&d, target.clone()),
        (&e, sibling.clone()),
    ];
    for (node, shape) in nodes {
        trees.set_remote(node.clone(), deleted_elsewhere(shape));
    }
    trees.settle();
    let nodes = [
        (&p, dir(&root(), "P")),
        (&a, folder),
        (&b, inside),
        (&d, target),
        (&e, sibling),
    ];
    for (node, shape) in nodes {
        let back = RemoteNode {
            meta_rev: 3,
            ..remote(shape)
        };
        trees.set_remote_made_here(node.clone(), back);
    }
    let results = (sent.iter())
        .map(|op| match op {
            op::Op::Delete(_) => outcome(op_result::Code::Superseded),
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    let result = CommitResult {
        seq: 3,
        results,
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.class(&b), Class::Remote);
    let next = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    let delete = op::Op::Delete(proto::Delete {
        node_id: e.clone(),
        requires: None,
    });
    assert_eq!(next, Some(vec![delete]));
}
