// SPDX-License-Identifier: Apache-2.0

//! The three trees and the classification table of docs/design/sync-engine.md sections 2.2 and 2.3.

use koidrive_engine::classify::{Class, SETTLE_GROUP_LIMIT, Trees, classify};
use koidrive_engine::ignore::RuleFile;
use koidrive_engine::names::{name_key, nfc};
use koidrive_engine::tree::{
    Content, FileIdentity, Kind, LocalNode, Names, RemoteNode, Shape, StateError, Tree, TreeError,
    same_content,
};
use koidrive_proto::v1::{NodeKind, NodeState, NodeStatus, VersionInfo};
use proptest::prelude::*;

const ROOT: &str = "root";

fn content(digest: u8, size: u64) -> Content {
    Content {
        size,
        manifest_digest: vec![digest; 32],
        mtime_ns: 1_794_268_740_123_456_789,
        ..Default::default()
    }
}

fn file(parent: &str, name: &str, digest: u8) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::File,
        content: Some(content(digest, 3)),
    }
}

fn dir(parent: &str, name: &str) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    }
}

fn remote(shape: Shape) -> RemoteNode {
    RemoteNode {
        shape,
        trashed: false,
        content_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 1,
        pending_for: None,
        pending_by: None,
    }
}

fn local(shape: Shape, ino: u128) -> LocalNode {
    LocalNode {
        shape,
        identity: FileIdentity {
            dev: 1,
            ino,
            birth_ns: Some(0),
        },
        parked: false,
    }
}

#[test]
fn contents_compare_by_size_digest_and_executable_bit_but_not_mtime() {
    let a = content(1, 3);
    let mut touched = a.clone();
    touched.mtime_ns += 1;
    assert!(same_content(&a, &touched));
    let mut executable = a.clone();
    executable.executable = true;
    assert!(!same_content(&a, &executable));
    assert!(!same_content(&a, &content(2, 3)));
    assert!(!same_content(&a, &content(1, 4)));
}

#[test]
fn the_classification_table_of_section_2_3() {
    let s = file(ROOT, "a", 1);
    let edited = file(ROOT, "a", 2);
    let moved = file(ROOT, "b", 1);
    let cases = [
        (Some(&s), Some(&s), Some(&s), Class::InSync),
        (Some(&s), Some(&s), Some(&edited), Class::Remote),
        (Some(&s), Some(&s), None, Class::Remote),
        (Some(&edited), Some(&s), Some(&s), Class::Local),
        (None, Some(&s), Some(&s), Class::Local),
        (Some(&edited), Some(&s), Some(&edited), Class::Same),
        (None, Some(&s), None, Class::Same),
        (Some(&edited), Some(&s), Some(&moved), Class::Concurrent),
        (None, Some(&s), Some(&moved), Class::Concurrent),
        (Some(&s), None, None, Class::Local),
        (None, None, Some(&s), Class::Remote),
        (Some(&s), None, Some(&edited), Class::Concurrent),
        (None, None, None, Class::InSync),
    ];
    for (i, (l, s, r, class)) in cases.into_iter().enumerate() {
        assert_eq!(classify(l, s, r), class, "case {i}");
    }
}

#[test]
fn a_tree_rejects_a_missing_parent_a_file_parent_and_a_cycle() {
    let mut tree: Tree<Shape> = Tree::new(ROOT);
    tree.insert("d".into(), dir(ROOT, "d"));
    tree.insert("f".into(), file("d", "f", 1));
    assert_eq!(tree.check(Names::ByKey), Ok(()));

    let mut orphan = tree.clone();
    orphan.insert("x".into(), file("gone", "x", 1));
    assert_eq!(
        orphan.check(Names::ByKey),
        Err(TreeError::MissingParent("x".into()))
    );

    let mut under_file = tree.clone();
    under_file.insert("x".into(), file("f", "x", 1));
    assert_eq!(
        under_file.check(Names::ByKey),
        Err(TreeError::ParentNotDir("x".into()))
    );

    let mut cycle = tree.clone();
    cycle.insert("p".into(), dir("q", "p"));
    cycle.insert("q".into(), dir("p", "q"));
    assert!(matches!(
        cycle.check(Names::ByKey),
        Err(TreeError::Cycle(_))
    ));

    let mut with_root = tree.clone();
    with_root.insert(ROOT.into(), dir(ROOT, ""));
    assert_eq!(with_root.check(Names::ByKey), Err(TreeError::RootStored));
}

#[test]
fn r_and_s_compare_names_by_key_while_l_compares_exact_names() {
    let mut tree: Tree<Shape> = Tree::new(ROOT);
    tree.insert("a".into(), file(ROOT, "Stra\u{df}e", 1));
    tree.insert("b".into(), file(ROOT, "STRASSE", 2));
    assert!(matches!(
        tree.check(Names::ByKey),
        Err(TreeError::NameTaken { .. })
    ));
    assert_eq!(tree.check(Names::Exact), Ok(()));
    tree.insert("c".into(), file(ROOT, "STRASSE", 3));
    assert!(matches!(
        tree.check(Names::Exact),
        Err(TreeError::NameTaken { .. })
    ));
}

#[test]
fn trashed_remote_nodes_count_as_absent() {
    let mut tree: Tree<RemoteNode> = Tree::new(ROOT);
    tree.insert("d".into(), remote(dir(ROOT, "d")));
    let mut trashed = remote(file(ROOT, "d", 1));
    trashed.trashed = true;
    tree.insert("t".into(), trashed);
    let mut child = remote(file("t", "c", 1));
    child.trashed = true;
    tree.insert("c".into(), child);
    assert_eq!(tree.check(Names::ByKey), Ok(()));
    assert!(tree.live("t").is_none());
    assert!(tree.get("t").is_some());
}

#[test]
fn reads_node_states_and_refuses_kinds_this_version_does_not_sync() {
    let state = NodeState {
        node_id: "n".into(),
        parent_id: ROOT.into(),
        name: "f".into(),
        kind: NodeKind::File as i32,
        status: NodeStatus::Trashed as i32,
        content_rev: 4,
        meta_rev: 6,
        meta_edit_rev: 0,
        trash_group: "g".into(),
        current: Some(VersionInfo {
            content: Some(content(1, 3)),
            ..Default::default()
        }),
        edit_rev: 0,
        pending_for: String::new(),
        pending_by: String::new(),
    };
    let (id, node) = RemoteNode::from_state(&state).unwrap();
    assert_eq!(id, "n");
    assert!(node.trashed);
    assert_eq!(
        (node.content_rev, node.meta_rev, node.meta_edit_rev),
        (4, 6, 6)
    );
    assert_eq!(node.shape, file(ROOT, "f", 1));
    let placed = NodeState {
        meta_edit_rev: 5,
        ..state.clone()
    };
    assert_eq!(RemoteNode::from_state(&placed).unwrap().1.meta_edit_rev, 5);

    let symlink = NodeState {
        kind: NodeKind::Symlink as i32,
        ..state.clone()
    };
    assert_eq!(
        RemoteNode::from_state(&symlink),
        Err(StateError::UnknownKind(NodeKind::Symlink as i32))
    );
    let empty = NodeState {
        current: None,
        ..state.clone()
    };
    assert_eq!(
        RemoteNode::from_state(&empty),
        Err(StateError::MissingContent)
    );
    let dir_state = NodeState {
        kind: NodeKind::Dir as i32,
        current: None,
        ..state
    };
    assert_eq!(
        RemoteNode::from_state(&dir_state).unwrap().1.shape,
        dir(ROOT, "f")
    );
}

#[test]
fn settling_records_matching_changes_and_drops_nodes_in_sync() {
    let mut trees = Trees::new(ROOT);
    let base = file(ROOT, "a", 1);
    let edited = file(ROOT, "a", 2);
    for id in ["same", "synced", "remote", "local"] {
        trees.set_synced(id.into(), Some(base.clone().with_name(id)));
        trees.set_remote(id.into(), remote(base.clone().with_name(id)));
        trees.set_local(id.into(), local(base.clone().with_name(id), 1));
    }
    trees.set_local("same".into(), local(edited.clone().with_name("same"), 1));
    trees.set_remote("same".into(), remote(edited.clone().with_name("same")));
    trees.set_remote("remote".into(), remote(edited.clone().with_name("remote")));
    trees.remove_local("local");

    trees.settle();

    let work = trees.work();
    assert_eq!(
        work,
        [
            ("local".to_owned(), Class::Local),
            ("remote".to_owned(), Class::Remote)
        ]
    );
    assert_eq!(trees.synced().get("same"), Some(&edited.with_name("same")));
    assert_eq!(trees.dirty().len(), 2);
    assert_eq!(trees.check(), Ok(()));
    trees.settle();
    assert_eq!(trees.work().len(), 2);
}

trait WithName {
    fn with_name(self, name: &str) -> Self;
}

impl WithName for Shape {
    fn with_name(mut self, name: &str) -> Self {
        self.name = name.into();
        self
    }
}

#[test]
fn names_compare_by_their_nfc_form_but_case_still_counts() {
    let s = file(ROOT, "\u{e9}", 1);
    let decomposed = file(ROOT, "e\u{301}", 1);
    assert!(decomposed.same_position(&s));
    assert_eq!(
        classify(Some(&decomposed), Some(&s), Some(&s)),
        Class::InSync
    );
    let upper = file(ROOT, "\u{c9}", 1);
    assert_eq!(classify(Some(&upper), Some(&s), Some(&s)), Class::Local);
}

#[test]
fn a_move_to_another_parent_under_the_same_name_is_a_change() {
    let s = file("p", "x", 1);
    let moved = file("q", "x", 1);
    assert!(!moved.same_position(&s));
    assert_eq!(classify(Some(&moved), Some(&s), Some(&s)), Class::Local);
    assert_eq!(classify(Some(&s), Some(&s), Some(&moved)), Class::Remote);
}

#[test]
fn settling_never_orphans_a_child_in_s() {
    let mut trees = Trees::new(ROOT);
    trees.set_synced("p".into(), Some(dir(ROOT, "p")));
    trees.set_synced("x".into(), Some(file("p", "x", 1)));
    trees.set_local("x".into(), local(file(ROOT, "a", 1), 1));
    trees.set_remote("x".into(), remote(file(ROOT, "b", 1)));
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.class("p"), Class::Same);
    assert_eq!(trees.class("x"), Class::Concurrent);

    trees.settle();

    let work = trees.work();
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(
        work,
        [
            ("p".to_owned(), Class::Same),
            ("x".to_owned(), Class::Concurrent)
        ]
    );
    assert!(trees.synced().get("p").is_some());

    trees.set_synced("x".into(), Some(file(ROOT, "b", 1)));
    trees.set_local("x".into(), local(file(ROOT, "b", 1), 1));
    trees.settle();
    assert_eq!(trees.work(), []);
    assert!(trees.synced().get("p").is_none());
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn settling_waits_for_the_updates_it_depends_on() {
    let mut trees = Trees::new(ROOT);
    trees.set_synced("p".into(), Some(dir(ROOT, "p")));
    trees.set_synced("x".into(), Some(file("p", "x", 1)));
    trees.set_local("x".into(), local(file(ROOT, "x", 1), 1));
    trees.set_remote("x".into(), remote(file(ROOT, "x", 1)));
    trees.set_local("q".into(), local(dir(ROOT, "q"), 2));
    trees.set_remote("q".into(), remote(dir(ROOT, "q")));
    trees.set_local("y".into(), local(file("q", "y", 1), 3));
    trees.set_remote("y".into(), remote(file("q", "y", 1)));
    trees.settle();
    assert_eq!(trees.work(), []);
    assert_eq!(trees.check(), Ok(()));
    assert!(trees.synced().get("p").is_none());
    assert_eq!(trees.synced().get("y"), Some(&file("q", "y", 1)));
}

#[test]
fn settling_never_gives_two_siblings_one_name_key() {
    let mut trees = Trees::new(ROOT);
    trees.set_synced("a".into(), Some(file(ROOT, "Stra\u{df}e", 1)));
    trees.set_local("a".into(), local(file(ROOT, "y", 1), 1));
    trees.set_remote("a".into(), remote(file(ROOT, "z", 1)));
    trees.set_synced("b".into(), Some(file(ROOT, "b", 2)));
    trees.set_local("b".into(), local(file(ROOT, "STRASSE", 2), 2));
    trees.set_remote("b".into(), remote(file(ROOT, "STRASSE", 2)));
    assert_eq!(trees.check(), Ok(()));
    trees.settle();
    assert_eq!(
        trees.work(),
        [
            ("a".to_owned(), Class::Concurrent),
            ("b".to_owned(), Class::Same)
        ]
    );
    assert_eq!(trees.check(), Ok(()));
}

/// Sets a node to `shape` in L and R, with `from` as its S shape.
fn same_change(trees: &mut Trees, id: &str, from: Shape, to: Shape, ino: u128) {
    trees.set_synced(id.into(), Some(from));
    trees.set_local(id.into(), local(to.clone(), ino));
    trees.set_remote(id.into(), remote(to));
}

#[test]
fn settling_takes_a_name_swap_made_on_both_sides_together() {
    let mut trees = Trees::new(ROOT);
    same_change(&mut trees, "a", file(ROOT, "x", 1), file(ROOT, "y", 1), 1);
    same_change(&mut trees, "b", file(ROOT, "y", 2), file(ROOT, "x", 2), 2);
    assert_eq!(trees.check(), Ok(()));
    trees.settle();
    assert_eq!(trees.work(), []);
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.synced().get("a"), Some(&file(ROOT, "y", 1)));
    assert_eq!(trees.synced().get("b"), Some(&file(ROOT, "x", 2)));
}

#[test]
fn settling_takes_a_three_way_rotation_made_on_both_sides_together() {
    let mut trees = Trees::new(ROOT);
    trees.set_synced("d".into(), Some(dir(ROOT, "d")));
    trees.set_local("d".into(), local(dir(ROOT, "d"), 9));
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    same_change(&mut trees, "a", file("d", "x", 1), file("d", "Y", 1), 1);
    same_change(&mut trees, "b", file("d", "y", 2), file("d", "z", 2), 2);
    same_change(&mut trees, "c", file("d", "z", 3), file("d", "x", 3), 3);
    assert_eq!(trees.check(), Ok(()));
    trees.settle();
    assert_eq!(trees.work(), []);
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.synced().get("a"), Some(&file("d", "Y", 1)));
    assert_eq!(trees.synced().get("b"), Some(&file("d", "z", 2)));
    assert_eq!(trees.synced().get("c"), Some(&file("d", "x", 3)));
}

#[test]
fn a_cycle_elsewhere_does_not_stop_a_swap_below_it() {
    let mut trees = Trees::new(ROOT);
    same_change(&mut trees, "a", dir("c", "x"), dir("c", "y"), 1);
    same_change(&mut trees, "b", dir("c", "y"), dir("c", "x"), 2);
    same_change(&mut trees, "c", dir(ROOT, "c"), dir("z", "c"), 3);
    trees.set_synced("z".into(), Some(dir("c", "z")));
    trees.set_local("z".into(), local(dir(ROOT, "local-z"), 4));
    trees.set_remote("z".into(), remote(dir(ROOT, "remote-z")));
    assert_eq!(trees.check(), Ok(()));
    trees.settle();
    assert_eq!(
        trees.work(),
        [
            ("c".to_owned(), Class::Same),
            ("z".to_owned(), Class::Concurrent)
        ]
    );
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.synced().get("a"), Some(&dir("c", "y")));
    assert_eq!(trees.synced().get("b"), Some(&dir("c", "x")));
}

#[test]
fn a_cycle_through_one_swap_does_not_stop_another_swap_that_fits() {
    let mut trees = Trees::new(ROOT);
    same_change(&mut trees, "a", dir(ROOT, "A"), dir("y", "A"), 1);
    same_change(&mut trees, "b", dir("y", "A"), dir(ROOT, "A"), 2);
    same_change(&mut trees, "y", dir("p", "x"), dir("q", "x"), 3);
    same_change(&mut trees, "z", dir("q", "x"), dir("p", "x"), 4);
    for id in ["p", "q"] {
        trees.set_synced(id.into(), Some(dir("a", id)));
        trees.set_local(id.into(), local(dir(ROOT, &format!("local-{id}")), 5));
        trees.set_remote(id.into(), remote(dir(ROOT, &format!("remote-{id}"))));
    }
    assert_eq!(trees.check(), Ok(()));
    trees.settle();
    assert_eq!(
        trees.work(),
        [
            ("a".to_owned(), Class::Same),
            ("b".to_owned(), Class::Same),
            ("p".to_owned(), Class::Concurrent),
            ("q".to_owned(), Class::Concurrent),
        ]
    );
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.synced().get("y"), Some(&dir("q", "x")));
    assert_eq!(trees.synced().get("z"), Some(&dir("p", "x")));
}

#[test]
fn a_create_and_a_move_into_it_settle_together_beside_a_cycle() {
    let mut trees = Trees::new(ROOT);
    same_change(&mut trees, "top", dir(ROOT, "x"), dir("new", "u"), 1);
    same_change(
        &mut trees,
        "old",
        file("mid", "x", 1),
        file("new", "x", 1),
        2,
    );
    trees.set_local("new".into(), local(dir("mid", "x"), 3));
    trees.set_remote("new".into(), remote(dir("mid", "x")));
    trees.set_synced("mid".into(), Some(dir("top", "x")));
    trees.set_local("mid".into(), local(dir(ROOT, "local-mid"), 4));
    trees.set_remote("mid".into(), remote(dir(ROOT, "x")));
    assert_eq!(trees.check(), Ok(()));
    trees.settle();
    assert_eq!(
        trees.work(),
        [
            ("mid".to_owned(), Class::Concurrent),
            ("top".to_owned(), Class::Same)
        ]
    );
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.synced().get("new"), Some(&dir("mid", "x")));
    assert_eq!(trees.synced().get("old"), Some(&file("new", "x", 1)));
}

/// Renames `n` files in one directory in a ring, the same way on both sides, so that no update
/// fits without all the others.
fn rotation(n: usize) -> Trees {
    let mut trees = Trees::new(ROOT);
    for i in 0..n {
        let from = file(ROOT, &format!("x{i}"), 1);
        let to = file(ROOT, &format!("x{}", (i + 1) % n), 1);
        same_change(&mut trees, &format!("f{i:02}"), from, to, i as u128);
    }
    trees
}

#[test]
fn a_group_larger_than_the_search_limit_stays_dirty_and_is_listed() {
    let mut trees = rotation(SETTLE_GROUP_LIMIT);
    trees.settle();
    assert_eq!(trees.work(), []);
    assert!(trees.unsearched().is_empty());

    let mut trees = rotation(SETTLE_GROUP_LIMIT + 1);
    trees.settle();
    let work = trees.work();
    assert_eq!(work.len(), SETTLE_GROUP_LIMIT + 1);
    assert!(work.iter().all(|(_, class)| *class == Class::Same));
    assert_eq!(trees.unsearched(), trees.dirty());
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_swap_that_would_clash_with_an_unsettled_sibling_waits() {
    let mut trees = Trees::new(ROOT);
    same_change(&mut trees, "a", file(ROOT, "x", 1), file(ROOT, "y", 1), 1);
    same_change(&mut trees, "b", file(ROOT, "y", 2), file(ROOT, "w", 2), 2);
    trees.set_synced("c".into(), Some(file(ROOT, "w", 3)));
    trees.set_local("c".into(), local(file(ROOT, "v", 3), 3));
    trees.set_remote("c".into(), remote(file(ROOT, "u", 3)));
    assert_eq!(trees.check(), Ok(()));
    trees.settle();
    assert_eq!(
        trees.work(),
        [
            ("a".to_owned(), Class::Same),
            ("b".to_owned(), Class::Same),
            ("c".to_owned(), Class::Concurrent)
        ]
    );
    assert_eq!(trees.check(), Ok(()));
}

/// The comparison of section 2.2 written out field by field, so that the properties do not test
/// `Shape::same` against itself.
fn equal(a: &Option<Shape>, b: &Option<Shape>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.parent == b.parent
                && nfc(&a.name) == nfc(&b.name)
                && a.kind == b.kind
                && match (&a.content, &b.content) {
                    (None, None) => true,
                    (Some(x), Some(y)) => {
                        x.size == y.size
                            && x.manifest_digest == y.manifest_digest
                            && x.executable == y.executable
                    }
                    _ => false,
                }
        }
        _ => false,
    }
}

fn shapes() -> impl Strategy<Value = Option<Shape>> {
    prop_oneof![
        Just(None),
        (0u8..3, 0u8..4, 0u8..2, any::<bool>()).prop_map(|(digest, name, parent, directory)| {
            let parent = ["p", "q"][parent as usize];
            let name = ["a", "A", "\u{e9}", "e\u{301}"][name as usize];
            Some(if directory {
                dir(parent, name)
            } else {
                file(parent, name, digest)
            })
        }),
    ]
}

/// One node of a generated space: its S shape, and what L and R did with it.
#[derive(Clone, Debug)]
enum Change {
    Keep,
    Delete,
    Edit(u8),
    Move { parent: usize, name: u8 },
}

fn changes() -> impl Strategy<Value = Change> {
    prop_oneof![
        3 => Just(Change::Keep),
        1 => Just(Change::Delete),
        1 => (0u8..3).prop_map(Change::Edit),
        2 => (0usize..8, 0u8..3).prop_map(|(parent, name)| Change::Move { parent, name }),
    ]
}

/// A node's shape after `change`, or ⊥. Parents always have a smaller index, so no tree has a
/// cycle, and a node whose parent is gone is gone too, so every tree is valid.
fn apply(i: usize, s: &[Option<Shape>], change: &Change, out: &[Option<Shape>]) -> Option<Shape> {
    let base = s[i].clone()?;
    let mut shape = match change {
        Change::Keep => base,
        Change::Delete => return None,
        Change::Edit(digest) if base.kind == Kind::File => Shape {
            content: Some(content(*digest, 3)),
            ..base
        },
        Change::Edit(_) => base,
        Change::Move { parent, name } => Shape {
            parent: if *parent < i && out[*parent].as_ref().is_some_and(|p| p.kind == Kind::Dir) {
                format!("n{parent}")
            } else {
                ROOT.into()
            },
            name: format!("{}{i}", ["n", "a", "b"][*name as usize]),
            ..base
        },
    };
    if shape.parent != ROOT {
        let parent: usize = shape.parent[1..].parse().unwrap();
        if out[parent].as_ref()?.kind != Kind::Dir {
            shape.parent = ROOT.into();
        }
    }
    Some(shape)
}

/// After a bootstrap from a server that lost commits, S forgets each node that R lacks or holds
/// live with another shape, and what S holds below it. A node in R's trash keeps S, so it stays a
/// remote delete, and so does a node R holds as S does.
#[test]
fn forgetting_what_the_server_lost_keeps_the_rest_of_s() {
    let mut trees = Trees::new(ROOT);
    let shapes = [
        ("albums", dir(ROOT, "albums")),
        ("photos", dir("albums", "photos")),
        ("one", file(ROOT, "one", 1)),
        ("two", file(ROOT, "two", 1)),
        ("old", file(ROOT, "old", 1)),
        ("same", file(ROOT, "same", 1)),
    ];
    for (ino, (id, shape)) in shapes.iter().enumerate() {
        trees.set_local(id.to_string(), local(shape.clone(), ino as u128));
        trees.set_synced(id.to_string(), Some(shape.clone()));
    }
    trees.set_remote("photos".into(), remote(dir(ROOT, "photos")));
    trees.set_remote("two".into(), remote(file(ROOT, "two", 0)));
    trees.set_remote(
        "old".into(),
        RemoteNode {
            trashed: true,
            ..remote(file(ROOT, "old", 1))
        },
    );
    trees.set_remote("same".into(), remote(file(ROOT, "same", 1)));
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        0,
        0,
    );
    assert_eq!(trees.check(), Ok(()));
    let classes = shapes.map(|(id, _)| (id, trees.class(id)));
    assert_eq!(
        classes,
        [
            ("albums", Class::Local),
            ("photos", Class::Concurrent),
            ("one", Class::Local),
            ("two", Class::Concurrent),
            ("old", Class::Remote),
            ("same", Class::InSync),
        ]
    );
}

/// A node R holds in the trash below a folder the server lost stays a remote delete. S forgets the
/// folder once the delete has taken the node out of it, and a node R lacks below it at once.
#[test]
fn a_remote_delete_below_a_folder_the_server_lost_stays_a_remote_delete() {
    let mut trees = Trees::new(ROOT);
    let shapes = [
        ("albums", dir(ROOT, "albums")),
        ("one", file("albums", "one", 1)),
        ("two", file("albums", "two", 1)),
    ];
    for (ino, (id, shape)) in shapes.iter().enumerate() {
        trees.set_local(id.to_string(), local(shape.clone(), ino as u128));
        trees.set_synced(id.to_string(), Some(shape.clone()));
    }
    trees.set_remote(
        "one".into(),
        RemoteNode {
            trashed: true,
            ..remote(file(ROOT, "one", 1))
        },
    );
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        0,
        0,
    );
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.class("one"), Class::Remote);
    assert_eq!(trees.class("two"), Class::Local);
    assert_eq!(
        trees.waiting().get("albums").map(|waiting| &waiting.shape),
        Some(&None)
    );
}

fn forgotten_after_a_loss() -> Trees {
    let mut trees = Trees::new(ROOT);
    let shape = file(ROOT, "b", 1);
    trees.set_local("a".into(), local(shape.clone(), 1));
    trees.set_synced("a".into(), Some(shape.clone()));
    trees.set_remote("a".into(), remote(shape));
    trees.remove_remote("a");
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        vec!["a".into()],
        Vec::new(),
        Vec::new(),
        Default::default(),
        0,
        0,
    );
    assert_eq!(trees.class("a"), Class::Local);
    trees
}

#[test]
fn a_node_another_device_creates_again_after_a_loss_takes_its_position() {
    let mut trees = forgotten_after_a_loss();
    trees.set_remote("a".into(), remote(file(ROOT, "c", 1)));
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.class("a"), Class::Remote);
}

#[test]
fn a_node_a_later_listing_holds_again_after_a_loss_takes_its_position() {
    let mut trees = forgotten_after_a_loss();
    trees.set_listed_remote("a".into(), remote(file(ROOT, "c", 1)));
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.class("a"), Class::Remote);
}

#[test]
fn a_node_another_device_creates_again_after_a_loss_with_other_content_is_not_taken() {
    let mut trees = forgotten_after_a_loss();
    trees.set_remote("a".into(), remote(file(ROOT, "c", 2)));
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.class("a"), Class::Concurrent);
}

fn a_folder_resent_after_a_loss_with_a_file_marked_as_nothing() -> Trees {
    a_folder_resent_after_a_loss_with_a_file_scanned_as(&[])
}

fn a_folder_resent_after_a_loss_with_a_file_scanned_as(digests: &[u8]) -> Trees {
    let mut trees = Trees::new(ROOT);
    for (id, shape, ino) in [
        ("p", dir(ROOT, "p"), 1),
        ("f", dir("p", "f"), 2),
        ("g", file("f", "g", 1), 3),
    ] {
        trees.set_local(id.into(), local(shape.clone(), ino));
        trees.set_synced(id.into(), Some(shape.clone()));
        trees.set_remote(id.into(), remote(shape));
    }
    for &digest in digests {
        trees.set_local_changed("g".into(), local(file("f", "g", digest), 3));
    }
    trees.settle();
    trees.remove_remote("g");
    trees.remove_remote("f");
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        vec!["f".into()],
        Vec::new(),
        Vec::new(),
        Default::default(),
        0,
        0,
    );
    assert_eq!(trees.class("g"), Class::Local);
    trees
}

fn deleted(shape: Shape) -> RemoteNode {
    RemoteNode {
        trashed: true,
        meta_rev: 2,
        trash_group: "2:0".into(),
        ..remote(shape)
    }
}

#[test]
fn a_file_a_loss_left_unmarked_does_not_keep_its_resent_folder_from_a_delete() {
    let mut trees = a_folder_resent_after_a_loss_with_a_file_marked_as_nothing();
    trees.set_remote("p".into(), deleted(dir(ROOT, "p")));
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.class("f"), Class::Remote);
}

#[test]
fn a_file_whose_edit_the_user_undid_does_not_keep_its_resent_folder_from_a_delete() {
    let mut trees = a_folder_resent_after_a_loss_with_a_file_scanned_as(&[2, 1]);
    trees.set_remote("p".into(), deleted(dir(ROOT, "p")));
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.class("f"), Class::Remote);
}

#[test]
fn a_file_the_user_makes_in_a_resent_folder_keeps_it_from_a_delete() {
    let mut trees = a_folder_resent_after_a_loss_with_a_file_marked_as_nothing();
    trees.set_local_changed("n".into(), local(file("f", "n", 2), 4));
    trees.set_remote("p".into(), deleted(dir(ROOT, "p")));
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.class("f"), Class::Local);
}

fn spaces() -> impl Strategy<Value = Vec<(bool, Change, Change)>> {
    proptest::collection::vec((any::<bool>(), changes(), changes()), 1..9)
}

proptest! {
    #![proptest_config(koidrive_test_support::regression_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/trees.proptest-regressions"),
        ProptestConfig::default().cases,
    ))]
    /// Balasubramaniam and Pierce, Definition 4.1.2 in ID form, against an independent equality:
    /// a side that did not change takes the other side's value, and a node changed on both sides is
    /// never settled by overwriting.
    #[test]
    fn classification_takes_only_the_side_that_changed(
        l in shapes(), s in shapes(), r in shapes()
    ) {
        let class = classify(l.as_ref(), s.as_ref(), r.as_ref());
        let expected = match (equal(&l, &s), equal(&r, &s)) {
            (true, true) => Class::InSync,
            (true, false) => Class::Remote,
            (false, true) => Class::Local,
            (false, false) if equal(&l, &r) => Class::Same,
            (false, false) => Class::Concurrent,
        };
        prop_assert_eq!(class, expected);
    }

    /// Settling valid trees leaves valid trees, takes every change made the same way on both sides
    /// that it can take without breaking S, leaves nothing in sync dirty, and is idempotent.
    #[test]
    fn settling_keeps_every_tree_valid(space in spaces()) {
        let mut s: Vec<Option<Shape>> = Vec::new();
        for (i, (directory, _, _)) in space.iter().enumerate() {
            let parent = (0..i).rev().find(|&j| s[j].as_ref().is_some_and(|p| p.kind == Kind::Dir));
            let parent = parent.map_or(ROOT.to_owned(), |j| format!("n{j}"));
            let name = format!("n{i}");
            s.push(Some(if *directory { dir(&parent, &name) } else { file(&parent, &name, 0) }));
        }
        let (mut l, mut r) = (Vec::new(), Vec::new());
        for (i, (_, local_change, remote_change)) in space.iter().enumerate() {
            let next_l = apply(i, &s, local_change, &l);
            l.push(next_l);
            let next_r = apply(i, &s, remote_change, &r);
            r.push(next_r);
        }
        let mut trees = Trees::new(ROOT);
        for i in 0..space.len() {
            let id = format!("n{i}");
            trees.set_synced(id.clone(), s[i].clone());
            if let Some(shape) = r[i].clone() {
                trees.set_remote(id.clone(), remote(shape));
            }
            if let Some(shape) = l[i].clone() {
                trees.set_local(id.clone(), local(shape, i as u128));
            }
        }
        prop_assert_eq!(trees.check(), Ok(()));
        trees.settle();
        let work = trees.work();
        prop_assert_eq!(trees.check(), Ok(()));
        for i in 0..space.len() {
            let id = format!("n{i}");
            let class = trees.class(&id);
            match work.iter().find(|(w, _)| *w == id) {
                Some((_, listed)) => {
                    prop_assert_eq!(*listed, class);
                    prop_assert_ne!(class, Class::InSync);
                }
                None => prop_assert_eq!(class, Class::InSync, "{} left the dirty set", id),
            }
        }
        let snapshot = trees.clone();
        trees.settle();
        prop_assert_eq!(trees.work(), work);
        prop_assert_eq!(trees, snapshot);
    }

    /// Settling only what changed since the last settle ends where settling every node ends, after
    /// each of a run of changes to L and R, in one tree or in both the same way, and to the rules,
    /// which change what is excluded without changing any node. A change that would leave L or R
    /// an invalid tree is not made, since neither the file system nor the server produces one.
    #[test]
    fn settling_what_changed_ends_where_settling_everything_ends(
        start in proptest::collection::vec(draws(), 1..8),
        steps in proptest::collection::vec(
            (proptest::collection::vec((any::<u8>(), 0u8..3, edits()), 1..4), 0usize..5),
            1..6,
        ),
    ) {
        let rule_files = [None, Some("x\n"), Some("y\n"), Some("x/\n"), Some("*\n")];
        let n = start.len();
        let mut trees = Trees::new(ROOT);
        for (i, shape) in draw_tree(&start).into_iter().enumerate() {
            let id = format!("n{i}");
            if let Some(shape) = &shape {
                trees.set_remote(id.clone(), remote(shape.clone()));
                trees.set_local(id.clone(), local(shape.clone(), i as u128));
            }
            trees.set_synced(id, shape);
        }
        trees.settle();
        for (step, rules) in steps {
            let rules = rule_files[rules];
            trees.set_rules(&[] as &[&str], rules.map(|rules| RuleFile::parse(rules.as_bytes())));
            for (node, side, edit) in step {
                let id = format!("n{}", node as usize % n);
                let shape = edit.map(|(parent, name, directory, digest)| {
                    let parent = match parent as usize % (n + 1) {
                        0 => ROOT.to_owned(),
                        p => format!("n{}", p - 1),
                    };
                    let name = ["x", "X", "y"][name as usize];
                    if directory { dir(&parent, name) } else { file(&parent, name, digest) }
                });
                if side != 1 {
                    let before = trees.local().get(&id).cloned();
                    match &shape {
                        Some(shape) => trees.set_local(id.clone(), local(shape.clone(), 0)),
                        None => trees.remove_local(&id),
                    }
                    if trees.check().is_err() {
                        match before {
                            Some(node) => trees.set_local(id.clone(), node),
                            None => trees.remove_local(&id),
                        }
                    }
                }
                if side != 0 {
                    let before = trees.remote().get(&id).cloned();
                    match shape {
                        Some(shape) => trees.set_remote(id.clone(), remote(shape)),
                        None => trees.remove_remote(&id),
                    }
                    if trees.check().is_err() {
                        match before {
                            Some(node) => trees.set_remote(id.clone(), node),
                            None => trees.remove_remote(&id),
                        }
                    }
                }
                prop_assert_eq!(trees.check(), Ok(()));
            }
            let mut everything = Trees::new(ROOT);
            everything.set_rules(&[] as &[&str], rules.map(|rules| RuleFile::parse(rules.as_bytes())));
            for (id, node) in trees.remote().iter() {
                everything.set_remote(id.clone(), node.clone());
            }
            for (id, node) in trees.local().iter() {
                everything.set_local(id.clone(), node.clone());
            }
            for (id, shape) in trees.synced().iter() {
                everything.set_synced(id.clone(), Some(shape.clone()));
            }
            trees.settle();
            everything.settle();
            prop_assert_eq!(trees.work(), everything.work());
            prop_assert_eq!(&trees, &everything);
        }
    }
}

/// A new shape for one node: a parent choice, a name, whether it is a directory and a digest, or ⊥.
fn edits() -> impl Strategy<Value = Option<(u8, u8, bool, u8)>> {
    prop_oneof![
        1 => Just(None),
        4 => (any::<u8>(), 0u8..3, any::<bool>(), 0u8..2).prop_map(Some),
    ]
}

/// One node's draw for one tree: ⊥, or its rank, a parent choice, a name and whether it is a
/// directory.
type Draw = Option<(u8, u8, u8, bool)>;

/// Builds a valid tree over `n0`, `n1`, ... from one [`Draw`] per node. A node may only go under a directory of lower rank, so the tree has
/// no cycle, but S and R are drawn with their own ranks, so their moves can swap, rotate and nest
/// into each other. A node whose drawn name key its parent already holds takes a name of its own.
fn draw_tree(draws: &[Draw]) -> Vec<Option<Shape>> {
    let mut order: Vec<usize> = (0..draws.len()).collect();
    order.sort_by_key(|&i| (draws[i].map(|draw| draw.0), i));
    let mut out: Vec<Option<Shape>> = vec![None; draws.len()];
    let mut placed: Vec<usize> = Vec::new();
    for i in order {
        let Some((_, parent, name, directory)) = draws[i] else {
            continue;
        };
        let parent = placed
            .get(parent as usize % (placed.len() + 1))
            .filter(|&&p| out[p].as_ref().is_some_and(|shape| shape.kind == Kind::Dir))
            .map_or(ROOT.to_owned(), |p| format!("n{p}"));
        let mut name = ["x", "X", "y"][name as usize].to_owned();
        if out
            .iter()
            .flatten()
            .any(|shape| shape.parent == parent && name_key(&shape.name) == name_key(&name))
        {
            name = format!("u{i}");
        }
        out[i] = Some(if directory {
            dir(&parent, &name)
        } else {
            file(&parent, &name, 0)
        });
        placed.push(i);
    }
    out
}

fn draws() -> impl Strategy<Value = Draw> {
    prop_oneof![
        1 => Just(None),
        6 => (any::<u8>(), any::<u8>(), 0u8..3, proptest::bool::weighted(0.7)).prop_map(Some),
    ]
}

fn searches() -> impl Strategy<Value = Vec<(Draw, Draw, u8)>> {
    proptest::collection::vec((draws(), draws(), 0u8..5), 1..9)
}

proptest! {
    #![proptest_config(koidrive_test_support::regression_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/trees.proptest-regressions"),
        4096,
    ))]

    /// Settling stops only when no non-empty set of the remaining Same updates can be made
    /// together with S still valid, checked by trying every such set.
    #[test]
    fn settling_leaves_no_set_of_same_updates_that_fits(space in searches()) {
        let s = draw_tree(&space.iter().map(|node| node.0).collect::<Vec<_>>());
        let r = draw_tree(&space.iter().map(|node| node.1).collect::<Vec<_>>());
        let mut trees = Trees::new(ROOT);
        for (i, (_, _, choice)) in space.iter().enumerate() {
            let id = format!("n{i}");
            trees.set_synced(id.clone(), s[i].clone());
            if let Some(shape) = r[i].clone() {
                trees.set_remote(id.clone(), remote(shape));
            }
            let l = match choice {
                0 => s[i].clone(),
                1 => Some(dir(ROOT, &format!("l{i}"))),
                _ => r[i].clone(),
            };
            if let Some(shape) = l {
                trees.set_local(id, local(shape, i as u128));
            }
        }
        prop_assert_eq!(trees.synced().check(Names::ByKey), Ok(()));
        prop_assert_eq!(trees.remote().check(Names::ByKey), Ok(()));
        trees.settle();
        let work = trees.work();
        prop_assert_eq!(trees.synced().check(Names::ByKey), Ok(()));
        prop_assert!(trees.unsearched().is_empty());
        let same: Vec<&String> = work
            .iter()
            .filter(|(_, class)| *class == Class::Same)
            .map(|(id, _)| id)
            .collect();
        for set in 1..1u32 << same.len() {
            let mut synced = trees.synced().clone();
            let chosen: Vec<&String> = same
                .iter()
                .enumerate()
                .filter(|(i, _)| set >> i & 1 == 1)
                .map(|(_, id)| *id)
                .collect();
            for id in &chosen {
                match trees.remote().live(id) {
                    Some(shape) => {
                        synced.insert((*id).clone(), shape.clone());
                    }
                    None => {
                        synced.remove(id);
                    }
                }
            }
            prop_assert!(
                synced.check(Names::ByKey).is_err(),
                "{:?} could still be settled together",
                chosen
            );
        }
    }
}
