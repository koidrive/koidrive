// SPDX-License-Identifier: Apache-2.0

use koidrive_store::Store;

fn preserved_rows(
    conn: &rusqlite::Connection,
) -> std::collections::BTreeMap<String, Vec<Vec<rusqlite::types::Value>>> {
    [
        ("link", "space_id, space_name, root, server_url, device_id, linked_at_ms, found_marker"),
        ("local_nodes", "node_id, parent_id, name, kind, content, ino, ino_high, birth_ns, parked"),
        ("synced_nodes", "node_id, parent_id, name, kind, content, pending_for"),
        ("remote_nodes", "node_id, parent_id, name, kind, status, content_rev, meta_rev, current, edit_rev, meta_edit_rev, trash_group, pending_for, pending_by"),
        ("engine_state", "id, id_generator"),
        ("node_sets", "node_set, node_id"),
        ("node_maps", "node_map, node_id, target"),
        ("remote_cursor", "id, history_id, last_seq, ahead_of_server, commit_id, committed_at_ms"),
        ("local_changes", "node_id"),
        ("edited_here", "node_id, seq"),
        ("moved_here", "node_id, seq, upgraded"),
        ("past_positions", "node_id, position, rev, parent_id, name"),
        ("replaced_versions", "node_id, position, version_id"),
        ("chunk_cache", "hash, size, last_used_ms"),
        ("commit_state", "id, accepted, halt, policy_code, cap, recovery_trigger, restore_seq, restore_point_ms"),
        ("commit_in_flight", "id, commit_message, result"),
        ("commit_slots", "device_seq, seq"),
        ("watched_contents", "node_id, changed"),
    ].into_iter().map(|(table, columns)| {
        let query = format!("SELECT {columns} FROM {table}");
        let count = conn.prepare(&query).unwrap().column_count();
        let order = (1..=count).map(|column| column.to_string()).collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare(&format!("{query} ORDER BY {order}")).unwrap();
        let rows = stmt.query_map([], |row| (0..count).map(|column| row.get(column)).collect::<rusqlite::Result<Vec<_>>>())
            .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        (table.to_owned(), rows)
    }).collect()
}

#[test]
fn additive_columns_do_not_change_preserved_release_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &path,
    )
    .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let before = preserved_rows(&conn);
    for table in before.keys() {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN future_upgrade_value INTEGER NOT NULL DEFAULT 7"
        ))
        .unwrap();
    }
    assert_eq!(preserved_rows(&conn), before);
}

macro_rules! upgrade_case {
    ($name:ident, |$store:ident| $body:block) => {
        #[test]
        fn $name() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.db");
            std::fs::copy(
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
                &path,
            )
            .unwrap();
            let before = rusqlite::Connection::open(&path).unwrap();
            let has_migration: bool = before
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM migrations WHERE name = ?1)",
                    [stringify!($name)],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                !has_migration,
                "fixture already contains {}",
                stringify!($name)
            );
            let preserved = preserved_rows(&before);
            for table in ["local_nodes", "synced_nodes", "remote_nodes", "engine_state", "local_changes", "replaced_versions"] {
                assert!(!preserved[table].is_empty(), "fixture has no {table}");
            }
            drop(before);
            let mut $store = Store::open(&path).unwrap();
            $store.transaction(|tx| {
                let link = tx.link()?.unwrap();
                assert_eq!(link.device_id, "fixture-device");
                let cursor = tx.cursor()?.unwrap();
                assert!(cursor.last_seq > 0);
                assert!(!cursor.commit_id.is_empty());
                let local = tx.local_nodes()?;
                let synced = tx.synced_nodes()?;
                let remote = tx.remote_nodes()?;
                let report = local.iter().find(|node| node.shape.name == "report.txt").unwrap();
                assert!(synced.iter().any(|node| node.node_id == report.node_id && node.shape.name == "report.txt"));
                assert!(remote.iter().any(|node| node.node_id == report.node_id && node.name == "report.txt" && node.content_rev > 1));
                assert!(local.iter().all(|node| node.shape.name != "original.txt"));
                let pending = local.iter().find(|node| node.shape.name == "pending.txt").unwrap();
                let old = synced.iter().find(|node| node.node_id == pending.node_id).unwrap();
                assert_ne!(pending.shape.content, old.shape.content);
                assert!(tx.engine_state()?.trees.local_changes.contains(&pending.node_id));
                Ok::<_, koidrive_store::Error>(())
            }).unwrap();
            assert_eq!(preserved_rows(&rusqlite::Connection::open(&path).unwrap()), preserved);
            if std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/migrations/",
                stringify!($name),
                ".sql"
            ))
            .is_file()
            {
                let after = rusqlite::Connection::open(&path).unwrap();
                let applied: bool = after
                    .query_row(
                        "SELECT EXISTS (SELECT 1 FROM migrations WHERE name = ?1)",
                        [stringify!($name)],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(applied, "migration {} did not run", stringify!($name));
            }
            $body
            let after_case = preserved_rows(&rusqlite::Connection::open(&path).unwrap());
            drop($store);
            drop(Store::open(&path).unwrap());
            assert_eq!(preserved_rows(&rusqlite::Connection::open(&path).unwrap()), after_case);
        }
    };
}

upgrade_case!(schema_alpha6_baseline, |store| {
    store
        .transaction(|tx| {
            let mut link = tx.link()?.unwrap();
            assert_eq!(link.device_id, "fixture-device");
            assert!(tx.cursor()?.unwrap().last_seq > 0);
            link.space_name = "Upgraded projects".into();
            tx.set_link(&link)?;
            assert_eq!(tx.link()?.unwrap().space_name, link.space_name);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});

include!("upgrade_cases/unseen_changes.rs");
