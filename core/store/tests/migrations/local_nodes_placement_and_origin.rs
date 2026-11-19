// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_placements_were_recorded_reads_every_local_node_as_placed_while_included_with_the_users_bytes()
 {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.put_local_node(&placed("a.txt", 1, true, true)))
        .unwrap();
    store_before(&path, "local_nodes_placement_and_origin");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(
        store.transaction(|tx| tx.local_nodes()).unwrap(),
        [placed("a.txt", 1, false, false)]
    );
}
