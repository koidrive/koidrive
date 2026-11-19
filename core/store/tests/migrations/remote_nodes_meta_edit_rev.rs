// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_meta_edit_revisions_reads_each_remote_node_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let node = NodeState {
        node_id: "n".into(),
        parent_id: "root".into(),
        name: "a".into(),
        kind: NodeKind::Dir as i32,
        status: NodeStatus::Live as i32,
        content_rev: 1,
        meta_rev: 3,
        meta_edit_rev: 2,
        ..Default::default()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.put_remote_node(&node))
        .unwrap();
    store_before(&path, "remote_nodes_meta_edit_rev");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let read = store.transaction(|tx| tx.remote_nodes()).unwrap();
    assert_eq!(
        read,
        [NodeState {
            meta_edit_rev: 0,
            ..node.clone()
        }]
    );
    store.transaction(|tx| tx.put_remote_node(&node)).unwrap();
    assert_eq!(store.transaction(|tx| tx.remote_nodes()).unwrap(), [node]);
}
