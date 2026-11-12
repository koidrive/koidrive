// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::ids::IdGen;
use koidrive_engine::names::{MAX_NAME_BYTES, name_key, normalize_name};
use koidrive_proto::v1::{
    ChunkRef, Commit, Content, CreateDir, CreateFile, Delete, Move, NodeKind, NodeState,
    NodeStatus, Op, PrepareUpload, Restore, UpdateContent, chunk_plan, op,
};
use koidrive_sim::space::{MAX_DEPTH, Role, Session, SpaceDo, node_states};
use proptest::prelude::*;
use sha2::{Digest, Sha256};

const NOW: i64 = 1_794_268_740_000;
const NAMES: &[&str] = &[
    "a",
    "A",
    "ß",
    "ss",
    "SS",
    "Straße",
    "strasse",
    "x.txt",
    "X.TXT",
    "Cafe\u{301}",
    "café",
    ".hidden",
];

#[derive(Clone, Debug)]
enum Kind {
    Mkdir,
    Create,
    Update,
    Move,
    Delete,
    Restore,
}

#[derive(Clone, Debug)]
struct Action {
    device: usize,
    lag: u64,
    kind: Kind,
    target: usize,
    parent: usize,
    name: usize,
    long: bool,
    text: u8,
}

fn action() -> impl Strategy<Value = Action> {
    let kind = prop_oneof![
        Just(Kind::Mkdir),
        Just(Kind::Create),
        Just(Kind::Update),
        Just(Kind::Move),
        Just(Kind::Delete),
        Just(Kind::Restore)
    ];
    (
        0..3usize,
        0..4u64,
        kind,
        any::<usize>(),
        any::<usize>(),
        0..NAMES.len(),
        proptest::bool::weighted(0.1),
        any::<u8>(),
    )
        .prop_map(
            |(device, lag, kind, target, parent, name, long, text)| Action {
                device,
                lag,
                kind,
                target,
                parent,
                name,
                long,
                text,
            },
        )
}

fn content(text: u8) -> Content {
    let bytes = [text; 3];
    let chunks = vec![ChunkRef {
        hash: Sha256::digest(bytes).to_vec(),
        size: 3,
        encoding: 0,
    }];
    let list: Vec<u8> = chunks
        .iter()
        .flat_map(|c| c.hash.iter().copied().chain(c.size.to_le_bytes()))
        .collect();
    Content {
        size: 3,
        chunk_count: 1,
        manifest_digest: Sha256::digest(list).to_vec(),
        chunks,
        ..Default::default()
    }
}

fn session(device: usize) -> Session {
    Session {
        device_id: format!("dev-{device}"),
        user_id: "user".into(),
        device_name: format!("Device {device}"),
        role: Role::Owner,
        max_format: 1,
    }
}

fn run(actions: &[Action]) -> Vec<NodeState> {
    let mut space = SpaceDo::new(11, NOW);
    let mut ids = IdGen::new(12);
    let root = space.space_id().to_string();
    let mut known = vec![root.clone()];
    for a in actions {
        let s = session(a.device);
        let pick = |i: usize| known[i % known.len()].clone();
        let name = if a.long {
            "n".repeat(MAX_NAME_BYTES - 1) + NAMES[a.name]
        } else {
            NAMES[a.name].to_string()
        };
        let fresh = ids.uuid_v7(NOW as u64);
        let body = match a.kind {
            Kind::Mkdir => op::Op::CreateDir(CreateDir {
                node_id: fresh.clone(),
                parent_id: pick(a.parent),
                name,
                pending_for: String::new(),
            }),
            Kind::Create => op::Op::CreateFile(CreateFile {
                node_id: fresh.clone(),
                parent_id: pick(a.parent),
                name,
                content: Some(content(a.text)),
            }),
            Kind::Update => op::Op::UpdateContent(UpdateContent {
                node_id: pick(a.target),
                content: Some(content(a.text)),
                resend: false,
            }),
            Kind::Move => op::Op::Move(Move {
                node_id: pick(a.target),
                new_parent_id: pick(a.parent),
                new_name: name,
                from: None,
                pending_for: String::new(),
            }),
            Kind::Delete => op::Op::Delete(Delete {
                node_id: pick(a.target),
                requires: None,
            }),
            Kind::Restore => op::Op::Restore(Restore {
                node_id: pick(a.target),
                ..Default::default()
            }),
        };
        let c = content(a.text);
        let plan = space
            .prepare_upload(
                &s,
                NOW,
                &PrepareUpload {
                    chunks: c.chunks.clone(),
                },
            )
            .unwrap();
        let receipts = if plan.chunks[0].action == chunk_plan::Action::Upload as i32 {
            vec![space.put_chunk(&c.chunks[0].hash, 3, NOW)]
        } else {
            vec![]
        };
        let commit = Commit {
            commit_id: ids.uuid_v7(NOW as u64),
            base_seq: space.head_seq().saturating_sub(a.lag),
            ops: vec![Op { op: Some(body) }],
            receipts,
            history_id: space.history_id().into(),
            device_seq: space.device_seq(&s.device_id) + 1,
            ..Default::default()
        };
        let done = space
            .commit(&s, NOW, &commit)
            .expect("valid commits are applied");
        known.push(fresh);
        for change in node_states(&done.result.changes) {
            assert_eq!(
                space.node(&change.node_id).as_ref(),
                Some(change),
                "a change record is the post-image"
            );
            if !known.contains(&change.node_id) {
                known.push(change.node_id.clone());
            }
        }
        let head = space.head_seq();
        assert_eq!(
            space.commit(&s, NOW, &commit).unwrap().result,
            done.result,
            "a retried commit returns the stored result"
        );
        assert_eq!(space.head_seq(), head);
        check_tree(&space, &root);
    }
    space.nodes()
}

fn check_tree(space: &SpaceDo, root: &str) {
    let nodes: BTreeMap<String, NodeState> = space
        .nodes()
        .into_iter()
        .map(|n| (n.node_id.clone(), n))
        .collect();
    let mut keys = BTreeSet::new();
    for node in nodes.values().filter(|n| n.node_id != root) {
        assert!(node.content_rev <= space.head_seq() && node.meta_rev <= space.head_seq());
        assert_eq!(
            normalize_name(&node.name).as_deref(),
            Some(node.name.as_str()),
            "stored names are valid NFC"
        );
        let parent = &nodes[&node.parent_id];
        assert_eq!(parent.kind(), NodeKind::Dir);
        let mut cur = parent;
        let mut depth = 1;
        while cur.node_id != root {
            assert_ne!(cur.node_id, node.node_id, "no cycles");
            cur = &nodes[&cur.parent_id];
            depth += 1;
            assert!(depth <= MAX_DEPTH);
        }
        if node.status() == NodeStatus::Live {
            assert_eq!(
                parent.status(),
                NodeStatus::Live,
                "a live node has a live parent"
            );
            assert!(
                keys.insert((node.parent_id.clone(), name_key(&node.name))),
                "live siblings have distinct name keys"
            );
            assert!(node.trash_group.is_empty());
        } else {
            assert!(!node.trash_group.is_empty());
        }
        assert_eq!(node.kind() == NodeKind::File, node.current.is_some());
    }
}

proptest! {
    #![proptest_config(koidrive_test_support::regression_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/space_properties.proptest-regressions"),
        256,
    ))]

    #[test]
    fn random_commits_keep_the_tree_valid_and_deterministic(actions in prop::collection::vec(action(), 1..60)) {
        let first = run(&actions);
        prop_assert_eq!(first, run(&actions));
    }
}
