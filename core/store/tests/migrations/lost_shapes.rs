// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_rename_pending_at_the_upgrade_to_lost_shapes_is_sent_as_a_create_beside_another_device_s_copy()
{
    let mut trees = Trees::new("root");
    let mut shape = engine_shape("root", "f");
    shape.content.as_mut().expect("a file").mtime_ns = 1_700_000_000_123_456_789;
    let renamed = EngineShape {
        name: "g".into(),
        ..shape.clone()
    };
    let remote = RemoteNode {
        shape: shape.clone(),
        trashed: false,
        content_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        edit_rev: 1,
        version_id: "v1".into(),
        made_over: Vec::new(),
        trash_group: String::new(),
        pending_for: None,
        pending_by: None,
    };
    let identity = FileIdentity {
        ino: 1,
        birth_ns: Some(0),
        handle: None,
    };
    trees.set_remote("f".into(), remote.clone());
    trees.set_local(
        "f".into(),
        EngineLocalNode {
            displaced: false,
            left_out_when_placed: false,
            content_from_server: false,
            shape: shape.clone(),
            identity,
            parked: false,
        },
    );
    trees.set_synced("f".into(), Some(shape.clone()));
    trees.settle(&Default::default());
    trees.set_local_changed(
        "f".into(),
        EngineLocalNode {
            displaced: false,
            left_out_when_placed: false,
            content_from_server: false,
            shape: renamed.clone(),
            identity,
            parked: false,
        },
    );
    trees.settle(&Default::default());
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        synced,
        Vec::new(),
        Vec::new(),
        Default::default(),
        &Default::default(),
        0,
        0,
    );
    trees.settle(&Default::default());
    assert!(trees.state().lost_shapes.contains_key("f"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.set_engine_state(&EngineState {
                trees: trees.state(),
                ..Default::default()
            })?;
            tx.put_local_node(&LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                node_id: "f".into(),
                shape: Shape {
                    parent_id: "root".into(),
                    name: "g".into(),
                    kind: NodeKind::File as i32,
                    content: shape.content.clone(),
                },
                identity: FileId::stored(1, Some(0), None),
                parked: false,
            })
        })
        .unwrap();
    store_before(&path, "lost_shapes");

    let state = Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.engine_state())
        .unwrap();
    assert!(state.trees.lost_shapes.is_empty());
    trees.restore(state.trees).unwrap();
    trees.rules_scanned();
    trees.set_remote(
        "n".into(),
        RemoteNode {
            version_id: "v2".into(),
            made_over: Vec::new(),
            ..remote
        },
    );
    trees.settle(&Default::default());
    let sent = Commits::default()
        .build(
            &mut trees,
            &mut IdGen::new(1),
            1_700_000_000_000,
            "history",
            "observer",
            2,
        )
        .cloned();
    assert!(
        sent.as_ref().is_some_and(|commit| {
            commit.ops.iter().any(|op| matches!(
            &op.op,
            Some(op::Op::CreateFile(file)) if file.node_id == "f" && file.content == shape.content
        ))
        }),
        "{sent:?}"
    );
}
