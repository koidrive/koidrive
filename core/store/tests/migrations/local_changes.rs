// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_local_changes_were_recorded_records_each_local_node_s_holds_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let local = |node_id: &str, shape: Shape, ino: u64| LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        node_id: node_id.into(),
        shape,
        identity: FileId::stored(ino.into(), None, None),
        parked: false,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            for (node_id, name) in [("same", "same.txt"), ("moved", "a.txt")] {
                tx.put_synced_node(&SyncedNode {
                    pending_for: String::new(),
                    node_id: node_id.into(),
                    shape: file_shape("root", name),
                })?;
            }
            tx.put_local_node(&local("same", file_shape("root", "same.txt"), 1))?;
            tx.put_local_node(&local("moved", file_shape("root", "b.txt"), 2))?;
            tx.put_local_node(&local("new", file_shape("root", "new.txt"), 3))?;
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "local_changes");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let state = store.transaction(|tx| tx.engine_state()).unwrap();
    assert_eq!(state.trees.local_changes, nodes(["moved", "new"]));
}

#[test]
fn a_store_from_before_local_changes_were_recorded_records_no_node_l_holds_as_s_does() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let local = |node_id: &str, shape: Shape, ino: u64| LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        node_id: node_id.into(),
        shape,
        identity: FileId::stored(ino.into(), None, None),
        parked: false,
    };
    let synced = file_shape("root", "m.txt");
    let mut touched = synced.clone();
    if let Some(content) = touched.content.as_mut() {
        content.mtime_ns += 1;
    }
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&SyncedNode {
                pending_for: String::new(),
                node_id: "name".into(),
                shape: file_shape("root", "\u{d55c}.txt"),
            })?;
            tx.put_local_node(&local(
                "name",
                file_shape("root", "\u{1112}\u{1161}\u{11ab}.txt"),
                1,
            ))?;
            tx.put_synced_node(&SyncedNode {
                pending_for: String::new(),
                node_id: "mtime".into(),
                shape: synced,
            })?;
            tx.put_local_node(&local("mtime", touched, 2))?;
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "local_changes");
    let mut store = Store::open(&path).unwrap();
    let state = store.transaction(|tx| tx.engine_state()).unwrap();
    assert_eq!(state.trees.local_changes, nodes::<0>([]));
}
