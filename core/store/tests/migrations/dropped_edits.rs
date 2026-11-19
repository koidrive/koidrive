// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_dropped_edits_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "dropped_edits");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.trees.dropped_edits.clear();
    assert!(!after.trees.dropped_edits.is_empty());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn an_edit_in_flight_in_a_store_from_before_dropped_edits_were_recorded_is_sent_again_after_a_loss()
{
    let shape = |name: &str, size| EngineShape {
        content: Some(content(size)),
        ..engine_shape("root", name)
    };
    let remote = |shape, rev, version_id: &str| RemoteNode {
        pending_by: None,
        pending_for: None,
        shape,
        trashed: false,
        content_rev: rev,
        meta_rev: rev,
        meta_edit_rev: rev,
        edit_rev: rev,
        trash_group: String::new(),
        version_id: version_id.into(),
        made_over: Vec::new(),
    };
    let local = |shape, ino| EngineLocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape,
        identity: FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
    };
    let mut trees = Trees::new("root");
    for (id, ino) in [("n", 1), ("p", 2)] {
        trees.set_remote(id.into(), remote(shape(id, 1), 1, "v1"));
        trees.set_local(id.into(), local(shape(id, 1), ino));
        trees.set_synced(id.into(), Some(shape(id, 1)));
    }
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("p".into(), local(shape("p", 2), 2));
    trees.settle(commits.aliases());
    assert!(
        commits
            .build(&mut trees, &mut ids, 1_794_268_740_000, "history", "me", 1)
            .is_some()
    );
    trees.set_remote("p".into(), remote(shape("p", 2), 2, "v2"));
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
    trees.set_remote("n".into(), remote(shape("b", 2), 3, "v2"));
    trees.set_local("n".into(), local(shape("b", 2), 1));
    trees.settle(commits.aliases());
    trees.set_local("n".into(), local(shape("b", 3), 1));
    trees.settle(commits.aliases());
    let edit = |op: &Op| {
        matches!(&op.op, Some(op::Op::UpdateContent(update))
            if update.node_id == "n" && update.content == Some(content(3)) && !update.resend)
    };
    let sent = commits.build(&mut trees, &mut ids, 1_794_268_740_001, "history", "me", 1);
    assert!(sent.is_some_and(|commit| commit.ops.len() == 1 && edit(&commit.ops[0])));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let saved = EngineState {
        trees: trees.state(),
        commits: commits.state(),
        ..Default::default()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&saved))
        .unwrap();
    store_before(&path, "dropped_edits");

    let mut store = Store::open(&path).unwrap();
    let stored = store.transaction(|tx| tx.engine_state()).unwrap();
    assert!(stored.trees.dropped_edits.is_empty());
    assert_eq!(stored, saved);
    trees.restore(stored.trees).unwrap();
    trees.rules_scanned();
    let mut commits = Commits::from_state(stored.commits).unwrap();
    let in_flight = commits.edited_in_flight();
    assert_eq!(in_flight, ["n"]);
    let welcome = Welcome {
        history_id: "history".into(),
        device_seq: 0,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
    trees.record_dropped_edits(in_flight);
    let dropped = EngineState {
        trees: trees.state(),
        commits: commits.state(),
        ..Default::default()
    };
    store
        .transaction(|tx| tx.update_engine_state(&saved, &dropped))
        .unwrap();
    drop(store);

    let stored = (Store::open(&path).unwrap())
        .transaction(|tx| tx.engine_state())
        .unwrap();
    assert_eq!(stored, dropped);
    trees.restore(stored.trees).unwrap();
    trees.rules_scanned();
    let mut commits = Commits::from_state(stored.commits).unwrap();
    let (trashed, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records(|seq| commits.lost_here(seq));
    trees.set_listed_remote("n".into(), remote(shape("n", 1), 1, "v1"));
    trees.set_listed_remote("p".into(), remote(shape("p", 1), 1, "v1"));
    trees.forget_lost(
        trashed,
        edited,
        synced,
        moved,
        untaken,
        records,
        &Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle(commits.aliases());
    let copy = |op: &Op| matches!(&op.op, Some(op::Op::CreateFile(create)) if create.content == Some(content(3)));
    let resent = commits.build(&mut trees, &mut ids, 1_794_268_740_002, "history", "me", 1);
    assert!(
        resent.is_some_and(|commit| commit.ops.iter().any(edit) && !commit.ops.iter().any(copy))
    );
}
