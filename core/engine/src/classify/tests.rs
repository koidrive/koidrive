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
            ino: ino as u128 + 1,
            birth_ns: Some(0),
        };
        let local = LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
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

fn a_server_folder_in_a_discarded_folder(folders: usize, waiting: bool) -> (Trees, Vec<NodeId>) {
    let mut trees = Trees::new("root");
    let mut ino = 0;
    let mut add = |trees: &mut Trees, id: String, shape: Shape, server: bool| {
        ino += 1;
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
        };
        if server {
            trees.set_remote(id.clone(), remote(shape.clone()));
            match waiting {
                true => trees.wait(id.clone(), Some(shape.clone()), Origin::Other),
                false => trees.set_synced(id.clone(), Some(shape.clone())),
            }
        }
        let local = LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
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
    let took = |folders, waiting| {
        let (trees, ids) = a_server_folder_in_a_discarded_folder(folders, waiting);
        let started = std::time::Instant::now();
        assert!(ids.iter().all(|id| trees.is_discarded(id)));
        assert!(trees.discard_state("f") == Some(Discard::Kept));
        started.elapsed()
    };
    for waiting in [false, true] {
        let (five_thousand, fifty_thousand) = (took(10, waiting), took(100, waiting));
        assert!(
            fifty_thousand < five_thousand * 30,
            "checking 50,000 discarded server copies took {fifty_thousand:?}, and 5,000 took {five_thousand:?}, with S updates waiting: {waiting}"
        );
    }
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
            ino,
            birth_ns: Some(0),
        };
        let local = LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
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

#[test]
fn a_discarded_node_drops_its_unsent_move_and_follows_the_server() {
    let mut trees = Trees::new("root");
    let shape = dir("root", "c");
    let local = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: shape.clone(),
        identity: FileIdentity {
            ino: 1,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("c".into(), local);
    trees.set_synced("c".into(), Some(shape.clone()));
    trees.set_remote(
        "c".into(),
        RemoteNode {
            trashed: true,
            ..remote(shape)
        },
    );
    trees.settle();
    trees.hold_unsent_move("c".into());
    assert_eq!(trees.class("c"), Class::Concurrent);
    trees.discard(["c".to_string()], &BTreeMap::new());
    assert_eq!(trees.class("c"), Class::Remote);
}

#[test]
fn a_resend_that_yields_to_the_server_trash_drops_its_unsent_move() {
    let mut trees = Trees::new("root");
    let shape = dir("root", "c");
    let local = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: shape.clone(),
        identity: FileIdentity {
            ino: 1,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("c".into(), local);
    trees.set_synced("c".into(), Some(shape.clone()));
    trees.set_remote(
        "c".into(),
        RemoteNode {
            trashed: true,
            ..remote(shape)
        },
    );
    trees.settle();
    trees.hold_unsent_move("c".into());
    trees.resends.insert("c".into());
    trees.yield_resends();
    assert_eq!(trees.class("c"), Class::Remote);
}

#[test]
fn a_resend_that_gave_way_to_a_delete_stays_a_resend_when_a_loss_follows() {
    let mut trees = Trees::new("root");
    let local = |shape: Shape, ino: u128| LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape,
        identity: FileIdentity {
            ino,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("f".into(), local(dir("root", "f"), 1));
    trees.set_synced("f".into(), Some(dir("root", "f")));
    trees.set_remote(
        "f".into(),
        RemoteNode {
            trashed: true,
            ..remote(dir("root", "f"))
        },
    );
    trees.set_local("c".into(), local(dir("f", "c"), 2));
    trees.settle();
    trees.resends.insert("c".into());
    trees.yield_resends();
    assert_eq!(trees.class("c"), Class::Remote);
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
        0,
        0,
    );
    trees.settle();
    assert!(!trees.lost_creates.contains("c"));
    assert_eq!(trees.class("c"), Class::Remote);
}

#[test]
fn a_resend_that_gave_way_to_a_delete_stays_a_resend_when_the_loss_also_takes_the_delete() {
    let mut trees = Trees::new("root");
    let local = |shape: Shape, ino: u128| LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape,
        identity: FileIdentity {
            ino,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("f".into(), local(dir("root", "f"), 1));
    trees.set_synced("f".into(), Some(dir("root", "f")));
    trees.set_remote(
        "f".into(),
        RemoteNode {
            trashed: true,
            ..remote(dir("root", "f"))
        },
    );
    trees.set_local("c".into(), local(dir("f", "c"), 2));
    trees.settle();
    trees.resends.insert("c".into());
    trees.yield_resends();
    assert_eq!(trees.class("c"), Class::Remote);
    trees.set_remote("f".into(), remote(dir("root", "f")));
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
        0,
        0,
    );
    trees.settle();
    assert!(trees.resends.contains("c"));
    assert!(!trees.lost_creates.contains("c"));
}

#[test]
fn a_discard_check_over_a_deep_chain_of_taken_along_server_folders_finishes() {
    let mut trees = Trees::new("root");
    let local = |shape: Shape, ino: u128, left_out_when_placed: bool| LocalNode {
        left_out_when_placed,
        content_from_server: false,
        shape,
        identity: FileIdentity {
            ino,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("f".into(), local(dir("root", "new"), 100, false));
    let mut parent = "root".to_string();
    for k in 0..40 {
        let id = format!("x{k}");
        let shape = dir(&parent, &id);
        trees.set_remote(id.clone(), remote(shape.clone()));
        let placed = if k == 0 { dir("f", &id) } else { shape };
        trees.set_local(id.clone(), local(placed, k + 1, k == 0));
        parent = id;
    }
    let shown = BTreeMap::from([("f".into(), dir("root", "new"))]);
    trees.discard(["f".into()], &shown);
    trees.discard_server_entries_below(&shown);
    assert!(trees.in_one_pass(|| trees.is_discarded("x0")));
}

#[test]
fn a_discard_check_over_a_deep_chain_of_kept_back_folders_finishes() {
    let mut trees = Trees::new("root");
    let mut parent = "root".to_string();
    for k in 0..40 {
        let (x, c) = (format!("x{k}"), format!("c{k}"));
        trees.set_remote(x.clone(), remote(dir(&parent, &x)));
        trees.set_remote(c.clone(), remote(dir(&x, &c)));
        let identity = FileIdentity {
            ino: k + 1,
            birth_ns: Some(0),
        };
        let local = LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: dir("root", &x),
            identity,
            parked: false,
        };
        trees.set_local(x.clone(), local);
        trees.discarded.insert(x.clone(), dir("root", &x));
        trees.discarded.insert(c.clone(), dir(&x, &c));
        parent = c;
    }
    trees.set_remote("f".into(), remote(dir(&parent, "f")));
    trees.others = BTreeSet::from(["f".into()]);
    assert!(trees.is_discarded("x0"));
}
