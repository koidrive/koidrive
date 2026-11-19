// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_found_markers_were_kept_reads_its_link_as_finding_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let link = Link {
        space_id: "0190f0a0-0000-7000-8000-000000000001".into(),
        space_name: "Projects".into(),
        root: std::path::PathBuf::from("/home/ada/Koidrive"),
        root_dev: 66306,
        root_volume: None,
        server_url: "https://files.example.com/".into(),
        device_id: "d1".into(),
        linked_at_ms: 1_700_000_000_000,
        found_marker: true,
    };
    let stored = || {
        Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.link())
            .unwrap()
            .unwrap()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_link(&link))
        .unwrap();
    store_before(&path, "found_marker");

    assert_eq!(
        stored(),
        Link {
            found_marker: false,
            ..link.clone()
        }
    );
    assert_eq!(applied(&path), all_migrations());
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_link(&link))
        .unwrap();
    assert_eq!(stored(), link);
}
