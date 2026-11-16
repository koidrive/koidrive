// SPDX-License-Identifier: Apache-2.0

use koidrive_store::Store;

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
            drop(before);
            let mut $store = Store::open(&path).unwrap();
            $store.transaction(|tx| {
                let link = tx.link()?.unwrap();
                assert_eq!(link.space_id, "space-alpha6");
                assert_eq!(link.space_name, "Projects");
                assert_eq!(link.device_id, "device-alpha6");
                let cursor = tx.cursor()?.unwrap();
                assert_eq!(cursor.history_id, "history-alpha6");
                assert_eq!(cursor.last_seq, 42);
                assert_eq!(cursor.commit_id, "commit-alpha6");
                assert_eq!(tx.chunk(&[7; 32])?.unwrap().size, 99);
                Ok::<_, koidrive_store::Error>(())
            }).unwrap();
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
            drop($store);
            drop(Store::open(&path).unwrap());
        }
    };
}

upgrade_case!(schema_alpha6_baseline, |store| {
    store
        .transaction(|tx| {
            assert_eq!(tx.link()?.unwrap().space_name, "Projects");
            assert_eq!(tx.cursor()?.unwrap().last_seq, 42);
            assert_eq!(tx.chunk(&[7; 32])?.unwrap().size, 99);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});

include!("upgrade_cases/unseen_changes.rs");
