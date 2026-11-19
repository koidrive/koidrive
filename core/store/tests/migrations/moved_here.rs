// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_moves_were_recorded_records_the_positions_its_commits_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    let node = |id: &str, name: &str, meta_rev: u64, status: NodeStatus| NodeState {
        node_id: id.into(),
        parent_id: "root".into(),
        name: name.into(),
        kind: NodeKind::File as i32,
        status: status as i32,
        content_rev: 1,
        meta_rev,
        current: Some(VersionInfo {
            content: Some(content(3)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let remote = [
        node("own", "own.txt", 7, NodeStatus::Live),
        node("other", "other.txt", 8, NodeStatus::Live),
        node("moved", "moved.txt", 9, NodeStatus::Live),
        node("gone", "gone.txt", 9, NodeStatus::Trashed),
        node("nfd", "\u{d55c}.txt", 7, NodeStatus::Live),
    ];
    let synced = [
        ("own", "own.txt"),
        ("other", "other.txt"),
        ("moved", "since.txt"),
        ("nfd", "\u{1112}\u{1161}\u{11ab}.txt"),
        ("gone", "gone.txt"),
    ]
    .map(|(id, name)| SyncedNode {
        node_id: id.into(),
        shape: file_shape("root", name),
        pending_for: String::new(),
    });
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.set_engine_state(&before)?;
            for node in &remote {
                tx.put_remote_node(node)?;
            }
            for node in &synced {
                tx.put_synced_node(node)?;
            }
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "moved_here");

    let mut store = Store::open(&path).unwrap();
    let after = before.clone();
    let upgraded = |seq| MovedHere {
        seq,
        upgraded: true,
    };
    before.trees.moved_here = BTreeMap::from([
        ("own".into(), upgraded(7)),
        ("moved".into(), upgraded(9)),
        ("nfd".into(), upgraded(7)),
    ]);
    before.trees.past_positions.clear();
    before.trees.lost_positions.clear();
    before.trees.lost_creates.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn an_own_move_acknowledged_before_the_upgrade_is_sent_again_after_a_later_loss() {
    let remote = |name: &str, meta_rev| RemoteNode {
        shape: engine_shape("root", name),
        trashed: false,
        content_rev: 1,
        meta_rev,
        meta_edit_rev: meta_rev,
        edit_rev: 0,
        trash_group: String::new(),
        version_id: String::new(),
        made_over: Vec::new(),
        pending_for: None,
        pending_by: None,
    };
    let local = |name: &str| EngineLocalNode {
        displaced: false,
        left_out_when_placed: false,
        content_from_server: false,
        shape: engine_shape("root", name),
        identity: FileIdentity {
            ino: 1,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
    };
    let mut trees = Trees::new("root");
    trees.set_remote("n".into(), remote("x", 1));
    trees.set_local("n".into(), local("x"));
    trees.set_synced("n".into(), Some(engine_shape("root", "x")));
    trees.set_local("n".into(), local("y"));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = commits.build(&mut trees, &mut ids, 1_794_268_740_000, "history", "me", 1);
    assert!(sent.is_some_and(|commit| matches!(
        &commit.ops[..],
        [Op {
            op: Some(op::Op::Move(_)),
            ..
        }]
    )));
    trees.set_remote("n".into(), remote("y", 2));
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    commits
        .received(CommitResult {
            seq: 2,
            results: vec![applied],
            ..Default::default()
        })
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle(commits.aliases());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let state = EngineState {
        trees: trees.state(),
        commits: commits.state(),
        ..Default::default()
    };
    let n = NodeState {
        node_id: "n".into(),
        parent_id: "root".into(),
        name: "y".into(),
        kind: NodeKind::File as i32,
        status: NodeStatus::Live as i32,
        content_rev: 1,
        meta_rev: 2,
        current: Some(VersionInfo {
            content: Some(content(3)),
            ..Default::default()
        }),
        ..Default::default()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.set_engine_state(&state)?;
            tx.put_remote_node(&n)?;
            tx.put_synced_node(&SyncedNode {
                node_id: "n".into(),
                shape: file_shape("root", "y"),
                pending_for: String::new(),
            })
        })
        .unwrap();
    store_before(&path, "moved_here");

    let state = (Store::open(&path).unwrap())
        .transaction(|tx| tx.engine_state())
        .unwrap();
    trees.restore(state.trees).unwrap();
    trees.rules_scanned();
    let mut commits = Commits::from_state(state.commits).unwrap();
    let welcome = Welcome {
        history_id: "history".into(),
        device_seq: 0,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
    let moved = trees.moved_here();
    trees.set_remote("n".into(), remote("x", 1));
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        moved,
        Vec::new(),
        Default::default(),
        &Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle(commits.aliases());
    let resent = commits.build(&mut trees, &mut ids, 1_794_268_740_001, "history", "me", 1);
    let moved_to_y = |op: &Op| matches!(&op.op, Some(op::Op::Move(moved)) if moved.new_name == "y");
    assert!(resent.is_some_and(|commit| commit.ops.iter().any(moved_to_y)));
}
