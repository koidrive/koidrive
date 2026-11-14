use super::*;
use crate::tree::FileIdentity;

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
        edit_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        pending_for: None,
        pending_by: None,
    }
}

fn waiting(shape: Shape) -> Waiting {
    Waiting {
        shape: Some(shape),
        origin: Origin::Commit(7),
    }
}

fn reclaimable(child_to: &str, own_wait: bool) -> Trees {
    let mut trees = Trees::new("root");
    let synced = [
        ("f", dir("root", "a")),
        ("c", dir("f", "child")),
        ("u", dir("root", "u")),
    ];
    for (ino, (id, shape)) in synced.into_iter().enumerate() {
        trees.set_synced(id.into(), Some(shape.clone()));
        let identity = FileIdentity {
            dev: 1,
            ino: ino as u128 + 1,
            birth_ns: Some(0),
        };
        let local = LocalNode {
            shape,
            identity,
            parked: false,
        };
        trees.set_local(id.into(), local);
    }
    let moved = [
        ("c", dir(child_to, "a")),
        ("u", dir("q", "u")),
        ("q", dir("root", "q")),
    ];
    for (id, shape) in moved {
        trees.set_remote(id.into(), remote(shape));
    }
    trees.settle();
    let mut state = trees.state();
    state.discarded.insert("f".into(), dir("root", "a"));
    state
        .waiting
        .insert("c".into(), waiting(dir(child_to, "a")));
    state.waiting.insert("u".into(), waiting(dir("q", "u")));
    if own_wait {
        state.waiting.insert("f".into(), waiting(dir("q", "a")));
    }
    trees.restore(state).unwrap();
    trees.rules_scanned();
    assert!(trees.reclaimed("f"));
    trees
}

#[test]
fn a_reclaimed_folder_leaves_s_with_only_the_waiting_update_its_removal_needs() {
    let mut trees = reclaimable("root", false);
    let unrelated = trees.waiting().get("u").cloned();
    assert_eq!(trees.reclaim(|_| true), ["f"]);
    assert_eq!(trees.synced().live("f"), None);
    assert_eq!(trees.synced().live("c"), Some(&dir("root", "a")));
    assert_eq!(trees.synced().live("u"), Some(&dir("root", "u")));
    assert_eq!(trees.waiting().get("u").cloned(), unrelated);
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_reclaimed_folders_own_waiting_update_does_not_keep_it_in_s() {
    let mut trees = reclaimable("root", true);
    assert_eq!(trees.reclaim(|_| true), ["f"]);
    assert_eq!(trees.synced().live("f"), None);
    assert!(!trees.waiting().contains_key("f"));
    assert!(trees.waiting().contains_key("u"));
}

#[test]
fn a_reclaim_that_cannot_keep_s_valid_changes_nothing() {
    let mut trees = reclaimable("q", true);
    let before = trees.state();
    assert_eq!(trees.reclaim(|_| true), Vec::<NodeId>::new());
    assert_eq!(trees.state(), before);
}
