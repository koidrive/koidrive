// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_nodes_born_as_temporary_files_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "born_as_temp");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.trees.born_as_temp.clear();
    assert!(!after.trees.born_as_temp.is_empty());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn a_store_from_before_temp_marks_marks_each_file_at_a_tmp_name_as_upgraded() {
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
            let files = [
                ("temp", "~WRD0000.tmp"),
                ("upper", "stage.TMP"),
                ("synced", "s.tmp"),
                ("dot", ".tmp"),
                ("plain", "report.docx"),
            ];
            for (ino, (node_id, name)) in (1..).zip(files) {
                tx.put_local_node(&local(node_id, file_shape("root", name), ino))?;
            }
            tx.put_local_node(&local("folder", dir_shape("root", "d.tmp"), 9))?;
            tx.put_synced_node(&SyncedNode {
                pending_for: String::new(),
                node_id: "synced".into(),
                shape: file_shape("root", "s.tmp"),
            })?;
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "born_as_temp");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let state = store.transaction(|tx| tx.engine_state()).unwrap();
    let upgraded = ["synced", "temp", "upper"].map(|id| (NodeId::from(id), TempBirth::Upgraded));
    assert_eq!(state.trees.born_as_temp, BTreeMap::from(upgraded));
}
