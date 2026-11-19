// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use koidrive_engine::state::{ContentProvenance, ContentValue, ContentVersion};
use koidrive_proto::v1::{Content, NodeKind, NodeState, VersionInfo};
use koidrive_store::{EngineChanges, EngineState, Store};

fn provenance() -> ContentProvenance {
    ContentProvenance {
        synced: Some(ContentVersion {
            version_id: "older-listing".into(),
            made_over: vec!["listing-base".into()],
            own: false,
        }),
        local: Some(ContentVersion {
            version_id: "actual-resent-value".into(),
            made_over: vec!["own-base".into(), "foreign-base".into()],
            own: true,
        }),
        edit_bases: (0..64).map(|n| format!("base-{n}")).collect(),
        unversioned_bases: (0..64)
            .map(|n| ContentValue {
                size: n,
                manifest_digest: vec![n as u8; 32],
                executable: n % 2 == 0,
            })
            .collect(),
    }
}

#[test]
fn ancestry_source_and_unsent_bases_survive_reopen_and_incremental_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = EngineState::default();
    before.trees.content_provenance = BTreeMap::from([("f".into(), provenance())]);
    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    let mut after = before.clone();
    after
        .trees
        .content_provenance
        .get_mut(&"f".into())
        .unwrap()
        .local = None;
    after
        .trees
        .content_provenance
        .get_mut(&"f".into())
        .unwrap()
        .edit_bases = vec!["new-base".into()];
    store
        .transaction(|tx| tx.write_engine_changes(&EngineChanges::between(&before, &after)))
        .unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
    store
        .transaction(|tx| {
            tx.write_engine_changes(&EngineChanges::between(&after, &EngineState::default()))
        })
        .unwrap();
    assert_eq!(
        store.transaction(|tx| tx.engine_state()).unwrap(),
        EngineState::default()
    );
}

#[test]
fn ancestry_upgrade_leaves_preexisting_values_without_guessed_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &path,
    )
    .unwrap();
    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert!(!tx.remote_nodes()?.is_empty());
            assert!(!tx.synced_nodes()?.is_empty());
            assert!(!tx.local_nodes()?.is_empty());
            assert!(tx.engine_state()?.trees.content_provenance.is_empty());
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
}

#[test]
fn ancestry_in_the_current_version_survives_remote_storage() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    let node = NodeState {
        node_id: "f".into(),
        name: "file".into(),
        kind: NodeKind::File as i32,
        current: Some(VersionInfo {
            version_id: "current".into(),
            made_over: (0..64).map(|n| format!("v{n}")).collect(),
            content: Some(Content::default()),
            ..VersionInfo::default()
        }),
        ..NodeState::default()
    };
    store
        .transaction(|tx| {
            tx.put_remote_node(&node)?;
            assert_eq!(tx.remote_nodes()?.as_slice(), std::slice::from_ref(&node));
            assert_eq!(tx.remote_current("f")?, node.current);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
}
