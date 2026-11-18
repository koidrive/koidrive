// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use proptest::prelude::*;

#[derive(Debug, Clone, Copy)]
struct Placed {
    version: u8,
    inode: u8,
    parked: bool,
}

#[derive(Debug, Clone)]
enum Op {
    Local(u8, Placed, bool, bool),
    LocalChanged(u8, Placed),
    RemoveLocal(u8),
    Synced(u8, Option<u8>),
    Mark(u8, Option<u8>),
    Remote(u8, u8, Option<u8>),
    RemoveRemote(u8),
    FromServer(u8, bool),
    LeftOut(u8, bool),
    Checkpoint,
}

fn id(i: u8) -> NodeId {
    format!("n{i}")
}

fn shape(i: u8, v: u8) -> Shape {
    Shape {
        parent: "root".into(),
        name: format!("{}{i}", ["a", "b", "A"][usize::from(v % 3)]),
        kind: Kind::File,
        content: Some(Content {
            size: u64::from(v / 3 % 2),
            ..Default::default()
        }),
    }
}

fn local(i: u8, placed: Placed, left_out: bool, from_server: bool) -> LocalNode {
    LocalNode {
        shape: shape(i, placed.version),
        identity: FileIdentity {
            ino: u128::from(i) + 1 + 100 * u128::from(placed.inode),
            birth_ns: Some(1),
        },
        parked: placed.parked,
        left_out_when_placed: left_out,
        content_from_server: from_server,
    }
}

fn remote(i: u8, v: u8, mark: Option<u8>) -> RemoteNode {
    RemoteNode {
        shape: shape(i, v),
        trashed: false,
        content_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        made_over: Vec::new(),
        edit_rev: 1,
        pending_for: mark.map(|m| format!("m{m}")),
        pending_by: None,
    }
}

fn op() -> impl Strategy<Value = Op> {
    let i = 0u8..4;
    let v = 0u8..6;
    let m = proptest::option::of(0u8..2);
    let placed = (v.clone(), 0u8..2, any::<bool>()).prop_map(|(version, inode, parked)| Placed {
        version,
        inode,
        parked,
    });
    prop_oneof![
        (i.clone(), placed.clone(), any::<bool>(), any::<bool>())
            .prop_map(|(i, p, a, b)| Op::Local(i, p, a, b)),
        (i.clone(), placed).prop_map(|(i, p)| Op::LocalChanged(i, p)),
        i.clone().prop_map(Op::RemoveLocal),
        (i.clone(), proptest::option::of(v.clone())).prop_map(|(i, v)| Op::Synced(i, v)),
        (i.clone(), m.clone()).prop_map(|(i, m)| Op::Mark(i, m)),
        (i.clone(), v.clone(), m.clone()).prop_map(|(i, v, m)| Op::Remote(i, v, m)),
        i.clone().prop_map(Op::RemoveRemote),
        (i.clone(), any::<bool>()).prop_map(|(i, b)| Op::FromServer(i, b)),
        (i.clone(), any::<bool>()).prop_map(|(i, b)| Op::LeftOut(i, b)),
        Just(Op::Checkpoint),
    ]
}

fn apply(trees: &mut Trees, op: &Op) {
    match *op {
        Op::Local(i, p, a, b) => trees.set_local(id(i), local(i, p, a, b)),
        Op::LocalChanged(i, p) => trees.set_local_changed(id(i), local(i, p, false, false)),
        Op::RemoveLocal(i) => trees.remove_local(&id(i)),
        Op::Synced(i, v) => trees.set_synced(id(i), v.map(|v| shape(i, v))),
        Op::Mark(i, m) => trees.set_synced_mark(id(i), m.map(|m| format!("m{m}"))),
        Op::Remote(i, v, m) => trees.set_remote(id(i), remote(i, v, m)),
        Op::RemoveRemote(i) => trees.remove_remote(&id(i)),
        Op::FromServer(i, b) => trees.set_content_from_server(&id(i), b),
        Op::LeftOut(i, b) => trees.set_left_out_when_placed(&id(i), b),
        Op::Checkpoint => {}
    }
}

type Local = BTreeMap<NodeId, LocalNode>;
type Synced = BTreeMap<NodeId, (Shape, Option<String>)>;

fn rows(trees: &Trees) -> (Local, Synced) {
    (
        trees
            .l
            .iter()
            .map(|(id, node)| (id.clone(), node.clone()))
            .collect(),
        trees
            .s
            .iter()
            .map(|(id, shape)| (id.clone(), (shape.clone(), trees.s_marks.get(id).cloned())))
            .collect(),
    )
}

fn differing<V: PartialEq>(
    before: &BTreeMap<NodeId, V>,
    after: &BTreeMap<NodeId, V>,
) -> BTreeSet<NodeId> {
    before
        .keys()
        .chain(after.keys())
        .filter(|id| before.get(*id) != after.get(*id))
        .cloned()
        .collect()
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/classify/checkpoint_changes.proptest-regressions"),
        512,
    ))]
    #[test]
    fn take_changed_names_every_node_whose_l_s_or_mark_changed(ops in proptest::collection::vec(op(), 1..40)) {
        let mut trees = Trees::new("root");
        trees.rules_scanned();
        trees.take_changed();
        let mut checkpointed = rows(&trees);
        for op in ops.iter().chain([&Op::Checkpoint]) {
            apply(&mut trees, op);
            if let Op::Checkpoint = op {
                trees.settle(&BTreeMap::new());
                let changed = trees.take_changed();
                let now = rows(&trees);
                prop_assert!(differing(&checkpointed.0, &now.0).is_subset(&changed.local), "after {:?}", ops);
                prop_assert!(differing(&checkpointed.1, &now.1).is_subset(&changed.synced), "after {:?}", ops);
                checkpointed = now;
            }
        }
    }
}
