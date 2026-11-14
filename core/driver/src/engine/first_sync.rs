// SPDX-License-Identifier: Apache-2.0

use koidrive_engine::effects::BatchLimit;
use koidrive_engine::tree::FileIdentity;
use koidrive_proto::v1::{Content, NodeKind, NodeState, NodeStatus, VersionInfo};
use koidrive_store::{Error, Store};

use super::{Engine, remote_node};
use crate::counting::allocated;

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;

fn remote(id: String, parent: &str, name: String, kind: NodeKind, size: u64) -> NodeState {
    let current = (kind == NodeKind::File).then(|| VersionInfo {
        content: Some(Content {
            size,
            manifest_digest: vec![7; 32],
            ..Default::default()
        }),
        ..Default::default()
    });
    NodeState {
        node_id: id,
        parent_id: parent.into(),
        name,
        kind: kind as i32,
        status: NodeStatus::Live as i32,
        content_rev: 1,
        meta_rev: 1,
        current,
        ..Default::default()
    }
}

fn first_sync(files: usize, per_dir: usize, size: u64, limit: BatchLimit) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("store.sqlite")).unwrap();
    store
        .transaction(|tx| -> Result<u64, Error> {
            let mut engine = Engine::load(tx, ROOT)?;
            engine.trees.rules_scanned();
            let dirs = files.div_ceil(per_dir);
            let folders = (0..dirs).map(|d| {
                remote(
                    format!("d{d:06}"),
                    ROOT,
                    format!("folder {d}"),
                    NodeKind::Dir,
                    0,
                )
            });
            let parent = |f: usize| {
                let group = f / per_dir;
                format!("d{group:06}")
            };
            let nodes = (0..files).map(|f| {
                remote(
                    format!("f{f:06}"),
                    &parent(f),
                    format!("file {f}"),
                    NodeKind::File,
                    size,
                )
            });
            for state in folders.chain(nodes) {
                tx.put_remote_node(&state)?;
                let (id, node) = remote_node(&state).unwrap();
                engine.trees.set_remote(id, node);
            }
            engine.checkpoint(tx)?;
            let before = allocated();
            let mut planned = 0;
            loop {
                let Engine {
                    trees,
                    effects,
                    ids,
                    ..
                } = &mut engine;
                let batch = effects.plan(trees, None, ids, NOW, limit).to_vec();
                if batch.is_empty() {
                    break;
                }
                engine.checkpoint(tx)?;
                for record in &batch {
                    planned += 1;
                    let identity = FileIdentity {
                        dev: 1,
                        ino: planned as u128,
                        birth_ns: Some(0),
                    };
                    (engine.effects.done(
                        &mut engine.trees,
                        None,
                        &record.effect_id,
                        Some(identity),
                    ))
                    .unwrap();
                }
                engine.checkpoint(tx)?;
            }
            let work = allocated() - before;
            assert!(engine.trees.dirty().is_empty(), "{:?}", engine.trees.work());
            assert_eq!(planned, files + dirs);
            assert_eq!(engine.trees.local().len(), files + dirs);
            let loaded = Engine::load(tx, ROOT)?;
            assert_eq!(loaded.trees.state(), engine.trees.state());
            assert_eq!(loaded.effects.state(), engine.effects.state());
            Ok(work)
        })
        .unwrap()
}

#[test]
fn a_first_sync_with_its_checkpoints_takes_work_in_proportion_to_its_files() {
    let runs = [
        (100, 3, BatchLimit::ONE),
        (4000, 3, BatchLimit::ONE),
        (100, 3, crate::disk::BATCH),
        (100, 4 << 20, crate::disk::BATCH),
    ];
    for (per_dir, size, limit) in runs {
        let small = first_sync(500, per_dir, size, limit);
        let large = first_sync(2000, per_dir, size, limit);
        let bytes = large as f64 / small as f64;
        assert!(
            bytes < 8.0,
            "{per_dir} per folder, {size} bytes a file, {limit:?}: 500 files allocated {small} bytes and 2000 files \
             {large}, {bytes:.1} times as many"
        );
    }
}
