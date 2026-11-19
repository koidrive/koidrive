// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn an_own_move_whose_earlier_positions_aged_out_before_the_upgrade_is_sent_again_after_a_loss() {
    let remote = |name: &str, meta_rev: u64, trashed: bool| RemoteNode {
        shape: engine_shape("root", name),
        trashed,
        content_rev: 1,
        meta_rev,
        meta_edit_rev: meta_rev,
        edit_rev: 0,
        trash_group: if trashed {
            format!("{meta_rev}:0")
        } else {
            String::new()
        },
        version_id: String::new(),
        made_over: Vec::new(),
        pending_for: None,
        pending_by: None,
    };
    let local = |name: &str| EngineLocalNode {
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
    trees.set_remote("n".into(), remote("x", 1, false));
    trees.set_local("n".into(), local("x"));
    trees.set_synced("n".into(), Some(engine_shape("root", "x")));
    trees.set_local("n".into(), local("y"));
    trees.set_remote("n".into(), remote("y", 2, false));
    trees.record_moved(&"n".into(), 2);
    trees.set_synced("n".into(), Some(engine_shape("root", "y")));
    trees.set_remote("n".into(), remote("y", 3, true));
    trees.set_remote("n".into(), remote("y", 4, false));
    let mut state = trees.state();
    state.past_positions.insert(
        "n".into(),
        vec![Agreed {
            rev: 2,
            parent: "root".into(),
            name: "y".into(),
        }],
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let n = NodeState {
        node_id: "n".into(),
        parent_id: "root".into(),
        name: "y".into(),
        kind: NodeKind::File as i32,
        status: NodeStatus::Live as i32,
        content_rev: 1,
        meta_rev: 4,
        current: Some(VersionInfo {
            content: Some(content(3)),
            ..Default::default()
        }),
        ..Default::default()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.set_engine_state(&EngineState {
                trees: state,
                ..Default::default()
            })?;
            tx.put_remote_node(&n)?;
            tx.put_synced_node(&SyncedNode {
                node_id: "n".into(),
                shape: file_shape("root", "y"),
                pending_for: String::new(),
            })
        })
        .unwrap();
    store_before(&path, "lost_positions");

    let state = (Store::open(&path).unwrap())
        .transaction(|tx| tx.engine_state())
        .unwrap();
    trees.restore(state.trees).unwrap();
    trees.rules_scanned();
    let moved = trees.moved_here();
    let synced = trees.taken_from_others(|_| false);
    trees.set_listed_remote("n".into(), remote("x", 1, false));
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        synced,
        moved,
        Vec::new(),
        Default::default(),
        &Default::default(),
        1,
        1,
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let resent = commits.build(&mut trees, &mut ids, 1_794_268_740_001, "history", "me", 1);
    let moved_to_y = |op: &Op| matches!(&op.op, Some(op::Op::Move(moved)) if moved.new_name == "y");
    assert!(resent.is_some_and(|commit| commit.ops.iter().any(moved_to_y)));
}
