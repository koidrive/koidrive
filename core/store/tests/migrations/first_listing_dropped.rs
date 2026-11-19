// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_the_first_listing_was_dropped_opens_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    Store::open(&path).unwrap();
    store_before(&path, "first_listing_dropped");
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO first_listing (history_id, live) VALUES ('h1', 1)",
            [],
        )
        .unwrap();

    Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let kept: i64 = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = 'first_listing'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(kept, 0);
}
