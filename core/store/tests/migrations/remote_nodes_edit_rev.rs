// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_edit_revisions_were_stored_reads_each_as_its_content_revision() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let node = NodeState {
        node_id: "f".into(),
        parent_id: "root".into(),
        name: "b.txt".into(),
        kind: NodeKind::File as i32,
        status: NodeStatus::Live as i32,
        content_rev: 4,
        meta_rev: 3,
        edit_rev: 2,
        ..Default::default()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.put_remote_node(&node))
        .unwrap();
    store_before(&path, "remote_nodes_edit_rev");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let nodes = store.transaction(|tx| tx.remote_nodes()).unwrap();
    assert_eq!(
        nodes,
        [NodeState {
            edit_rev: 4,
            ..node
        }]
    );
}
