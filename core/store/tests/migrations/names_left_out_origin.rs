// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn name_origins_survive_upgrade_updates_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    let unsent = NodeId::from("unsent");
    before
        .trees
        .names_left_out
        .insert(unsent.clone(), NameLeftOut::DiskRefusal(None));
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "names_left_out_origin");
    assert!(!applied(&path).contains("names_left_out_origin"));

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    let mut after = before.clone();
    after.trees.names_left_out.insert(
        "claimant".into(),
        NameLeftOut::FrozenHolder(RenamedOver {
            parent: "p".into(),
            name: "A".into(),
        }),
    );
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let conn = Connection::open(&path).unwrap();
    for (origin, parent, name) in [
        ("invented", Some("p"), Some("A")),
        ("frozen_holder", None, None),
        ("frozen_holder", Some("p"), None),
    ] {
        assert!(matches!(
            conn.execute(
                "INSERT INTO names_left_out (node_id, origin, sent_parent_id, sent_name)
                 VALUES ('invalid', ?1, ?2, ?3)",
                rusqlite::params![origin, parent, name],
            ),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation
        ));
    }
    drop(conn);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}
