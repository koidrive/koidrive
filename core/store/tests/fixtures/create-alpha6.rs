// SPDX-License-Identifier: Apache-2.0

use koidrive_store::{Cursor, Link, Store};

#[test]
fn create_fixture() {
    let path = std::env::var("KOIDRIVE_FIXTURE_PATH").unwrap();
    let mut store = Store::open(path).unwrap();
    store
        .transaction(|tx| {
            tx.set_link(&Link {
                space_id: "space-alpha6".into(),
                space_name: "Projects".into(),
                root: "/fixture/Projects".into(),
                root_dev: 17,
                server_url: "https://example.invalid".into(),
                device_id: "device-alpha6".into(),
                linked_at_ms: 1234,
                found_marker: false,
            })?;
            tx.set_cursor(&Cursor {
                history_id: "history-alpha6".into(),
                last_seq: 42,
                commit_id: "commit-alpha6".into(),
                committed_at_ms: 5678,
                ahead_of_server: false,
            })?;
            tx.put_chunk(&[7; 32], 99, 3456)?;
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
}
