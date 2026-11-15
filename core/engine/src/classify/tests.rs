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

fn a_server_folder_in_a_discarded_folder(folders: usize) -> (Trees, Vec<NodeId>) {
    let mut trees = Trees::new("root");
    let mut ino = 0;
    let mut add = |trees: &mut Trees, id: String, shape: Shape, server: bool| {
        ino += 1;
        let identity = FileIdentity {
            dev: 1,
            ino,
            birth_ns: Some(0),
        };
        if server {
            trees.set_remote(id.clone(), remote(shape.clone()));
            trees.set_synced(id.clone(), Some(shape.clone()));
        }
        let local = LocalNode {
            shape,
            identity,
            parked: false,
        };
        trees.set_local(id, local);
    };
    add(&mut trees, "f".into(), dir("root", "new"), false);
    add(&mut trees, "x".into(), dir("f", "ignored"), true);
    let mut ids = vec!["x".to_string()];
    for i in 0..folders {
        let folder = format!("d{i}");
        add(&mut trees, folder.clone(), dir("x", &folder), true);
        ids.push(folder.clone());
        for j in 0..500 {
            let content = Content {
                size: 1,
                mtime_ns: 0,
                executable: false,
                chunks: Vec::new(),
                chunk_count: 0,
                manifest_digest: vec![1; 32],
                chunker: String::new(),
            };
            let shape = Shape {
                parent: folder.clone(),
                name: format!("f{j}"),
                kind: Kind::File,
                content: Some(content),
            };
            let id = format!("c{i}-{j}");
            add(&mut trees, id.clone(), shape, true);
            ids.push(id);
        }
    }
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored/\n")));
    trees.discard_server_entries_below(&BTreeMap::from([("f".into(), dir("root", "new"))]));
    trees.discard(
        ["f".to_string()],
        &BTreeMap::from([("f".into(), dir("root", "new"))]),
    );
    assert_eq!(trees.discarded.len(), ids.len() + 1);
    (trees, ids)
}

#[test]
fn a_server_folder_of_fifty_thousand_files_in_a_discarded_folder_is_checked_in_linear_time() {
    let took = |folders| {
        let (trees, ids) = a_server_folder_in_a_discarded_folder(folders);
        let started = std::time::Instant::now();
        assert!(ids.iter().all(|id| trees.is_discarded(id)));
        assert!(trees.discard_state("f") == Some(Discard::Kept));
        started.elapsed()
    };
    let (five_thousand, fifty_thousand) = (took(10), took(100));
    assert!(
        fifty_thousand < five_thousand * 30,
        "checking 50,000 discarded server copies took {fifty_thousand:?}, and 5,000 took {five_thousand:?}"
    );
}

#[test]
fn two_server_folders_swapped_below_a_discarded_folder_stay_discarded() {
    let mut trees = Trees::new("root");
    let mut ino = 0;
    let mut place = |trees: &mut Trees, id: &str, shape: Shape, server: bool| {
        ino += 1;
        if server {
            trees.set_remote(id.into(), remote(shape.clone()));
        }
        let identity = FileIdentity {
            dev: 1,
            ino,
            birth_ns: Some(0),
        };
        let local = LocalNode {
            shape,
            identity,
            parked: false,
        };
        trees.set_local(id.into(), local);
    };
    place(&mut trees, "f", dir("root", "new"), false);
    for (id, shape) in [
        ("x", dir("f", "ignored")),
        ("y", dir("x", "y")),
        ("z", dir("y", "z")),
    ] {
        place(&mut trees, id, shape.clone(), true);
        trees.set_synced(id.into(), Some(shape));
    }
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored/\n")));
    let shown = BTreeMap::from([("f".into(), dir("root", "new"))]);
    trees.discard_server_entries_below(&shown);
    trees.discard(["f".to_string()], &shown);
    place(&mut trees, "z", dir("x", "z"), true);
    place(&mut trees, "y", dir("z", "y"), true);
    assert!(["x", "y", "z"].iter().all(|id| trees.is_discarded(id)));
    assert!(trees.discard_state("f") == Some(Discard::Kept));
}
