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
        made_over: Vec::new(),
        pending_for: None,
        pending_by: None,
    }
}

fn ancestry_file(value: u8) -> Shape {
    Shape {
        parent: "root".into(),
        name: "f".into(),
        kind: Kind::File,
        content: Some(Content {
            size: 1,
            manifest_digest: vec![value; 32],
            ..Content::default()
        }),
    }
}

fn ancestry_remote(value: u8, version: &str, bases: &[&str]) -> RemoteNode {
    RemoteNode {
        version_id: version.into(),
        made_over: bases.iter().map(|id| (*id).into()).collect(),
        content_rev: 10,
        edit_rev: 10,
        ..remote(ancestry_file(value))
    }
}

fn ancestry_tree() -> Trees {
    let mut trees = Trees::new("root");
    trees.set_remote("f".into(), ancestry_remote(1, "held", &["foreign-base"]));
    trees.set_local(
        "f".into(),
        LocalNode {
            shape: ancestry_file(1),
            identity: FileIdentity {
                ino: 1,
                birth_ns: Some(0),
            },
            parked: false,
            left_out_when_placed: false,
            content_from_server: false,
        },
    );
    trees.set_synced("f".into(), Some(ancestry_file(1)));
    trees.set_content_from_server("f", true);
    trees
}

fn ancestry_write(trees: &mut Trees, value: u8) {
    let mut local = trees.l.get("f").unwrap().clone();
    local.shape = ancestry_file(value);
    trees.set_local_changed("f".into(), local);
}

#[test]
fn ancestry_of_an_edit_stops_at_its_actual_foreign_base() {
    let mut trees = ancestry_tree();
    trees.set_remote("f".into(), ancestry_remote(3, "unheld", &["unrelated"]));
    ancestry_write(&mut trees, 2);
    assert_eq!(trees.edit_bases("f"), ["held"]);
    assert_eq!(trees.source_version_id("f"), None);
    ancestry_write(&mut trees, 4);
    assert_eq!(trees.edit_bases("f"), ["held"]);
}

#[test]
fn ancestry_of_own_edits_includes_held_versions_but_no_uncommitted_ids() {
    let mut trees = ancestry_tree();
    ancestry_write(&mut trees, 2);
    trees.set_remote_edited_here("f".into(), ancestry_remote(2, "own-2", &["held"]));
    ancestry_write(&mut trees, 3);
    ancestry_write(&mut trees, 4);
    assert_eq!(trees.edit_bases("f"), ["own-2", "held"]);
    trees.set_remote_edited_here("f".into(), ancestry_remote(3, "own-3", &["own-2", "held"]));
    assert_eq!(trees.edit_bases("f"), ["own-3", "own-2", "held"]);
}

#[test]
fn ancestry_does_not_recover_an_old_own_base_after_a_foreign_download() {
    let mut trees = ancestry_tree();
    ancestry_write(&mut trees, 2);
    ancestry_write(&mut trees, 3);
    trees.set_remote("f".into(), ancestry_remote(4, "foreign-4", &[]));
    let mut local = trees.l.get("f").unwrap().clone();
    local.shape = ancestry_file(4);
    trees.set_local("f".into(), local);
    trees.set_synced("f".into(), Some(ancestry_file(4)));
    trees.set_content_from_server("f", true);
    ancestry_write(&mut trees, 5);
    trees.set_remote_edited_here("f".into(), ancestry_remote(2, "late-own-2", &["held"]));
    assert_eq!(trees.edit_bases("f"), ["foreign-4"]);
}

#[test]
fn ancestry_record_give_way_uses_the_resent_value_not_an_older_listing() {
    let mut trees = ancestry_tree();
    trees.set_listed_remote("f".into(), ancestry_remote(0, "older-listing", &[]));
    trees.set_synced("f".into(), Some(ancestry_file(0)));
    trees.resends.insert("f".into());
    let before = trees.state();
    trees.restore(before).unwrap();
    trees.set_remote(
        "f".into(),
        ancestry_remote(2, "unrelated", &["older-listing"]),
    );
    assert!(trees.resends.contains("f"));
    assert_eq!(trees.source_version_id("f"), Some("held"));
    trees.set_remote("f".into(), ancestry_remote(3, "continuation", &["held"]));
    assert!(!trees.resends.contains("f"));
    assert_eq!(trees.s.get("f"), Some(&ancestry_file(1)));
}

#[test]
fn ancestry_listing_give_way_uses_the_snapshot_source() {
    for carries_source in [false, true] {
        let mut trees = ancestry_tree();
        let records = trees.remote_records(|_| false);
        let synced = trees.taken_from_others(|_| false);
        let bases = if carries_source {
            vec!["held"]
        } else {
            vec!["foreign-base"]
        };
        trees.set_listed_remote("f".into(), ancestry_remote(2, "after-loss", &bases));
        trees.forget_lost(
            vec![],
            vec![],
            synced,
            vec![],
            vec![],
            records,
            &Default::default(),
            0,
            0,
        );
        assert_eq!(trees.s.get("f").is_some(), carries_source);
        assert_eq!(trees.source_version_id("f"), Some("held"));
    }
}

#[test]
fn ancestry_a_reused_sequence_does_not_discard_a_known_later_value() {
    let mut trees = ancestry_tree();
    let records = trees.remote_records(|_| false);
    let synced = trees.taken_from_others(|_| false);
    trees.set_listed_remote("f".into(), ancestry_remote(0, "foreign-base", &[]));
    trees.forget_lost(
        vec![],
        vec![],
        synced,
        vec![],
        vec![],
        records,
        &Default::default(),
        10,
        0,
    );
    trees.settle(&Default::default());
    let mut commits = crate::commit::Commits::default();
    let commit = commits.build(
        &mut trees,
        &mut crate::ids::IdGen::new(1),
        1_794_268_740_000,
        "history",
        "device",
        10,
    );
    assert!(commit.is_some_and(|commit| matches!(commit.ops.as_slice(), [op]
        if matches!(&op.op, Some(koidrive_proto::v1::op::Op::UpdateContent(update))
            if update.resend && update.source_version_id == "held" && update.content == ancestry_file(1).content))), "{commit:?}");
}

#[test]
fn ancestry_unknown_relationships_keep_sequence_based_survival() {
    for matching_source in [false, true] {
        let mut trees = ancestry_tree();
        let mut records = trees.remote_records(|_| false);
        let synced = trees.taken_from_others(|_| false);
        let listed = if matching_source {
            "unrelated"
        } else {
            records.get_mut("f").unwrap().source_version_id = Some("other-source".into());
            "foreign-base"
        };
        trees.set_listed_remote("f".into(), ancestry_remote(0, listed, &[]));
        trees.forget_lost(
            vec![],
            vec![],
            synced,
            vec![],
            vec![],
            records,
            &Default::default(),
            10,
            0,
        );
        assert_eq!(trees.s.get("f"), Some(&ancestry_file(1)));
        assert!(!trees.resends.contains("f"));
    }
}

#[test]
fn ancestry_old_store_without_provenance_does_not_guess_a_base() {
    let mut trees = ancestry_tree();
    let mut state = trees.state();
    state.content_provenance.clear();
    trees.restore(state).unwrap();
    ancestry_write(&mut trees, 2);
    assert!(trees.edit_bases("f").is_empty());
}

#[test]
fn ancestry_is_newest_first_deduplicated_and_bounded() {
    let mut trees = ancestry_tree();
    for n in 2..100 {
        ancestry_write(&mut trees, n);
        let mut node = ancestry_remote(n, &format!("own-{n}"), &[]);
        node.made_over = trees
            .edit_bases("f")
            .into_iter()
            .flat_map(|base| [base.clone(), base])
            .collect();
        trees.set_remote_edited_here("f".into(), node);
    }
    ancestry_write(&mut trees, 100);
    let bases = trees.edit_bases("f");
    assert_eq!(bases.len(), 64);
    assert_eq!(&bases[..3], ["own-99", "own-98", "own-97"]);
    assert_eq!(bases.last().unwrap(), "own-36");
}

#[test]
fn ancestry_does_not_claim_unheld_transitive_foreign_values_as_own_bases() {
    let mut trees = ancestry_tree();
    ancestry_write(&mut trees, 2);
    trees.set_remote_edited_here(
        "f".into(),
        ancestry_remote(2, "own-2", &["held", "foreign-base"]),
    );
    ancestry_write(&mut trees, 3);
    assert_eq!(trees.edit_bases("f"), ["own-2", "held"]);
}

#[test]
fn ancestry_restore_rejects_an_unbounded_or_ambiguous_chain() {
    let mut trees = ancestry_tree();
    let before = trees.state();
    for bases in [
        vec!["v".to_owned(); 65],
        vec!["v".to_owned(), "v".to_owned()],
        vec![String::new()],
    ] {
        let mut state = before.clone();
        state.content_provenance.get_mut("f").unwrap().edit_bases = bases;
        assert_eq!(
            trees.restore(state),
            Err(StateError::InvalidContentProvenance("f".into()))
        );
    }
}

#[test]
fn ancestry_incoming_version_chain_is_retained_with_a_bound() {
    use koidrive_proto::v1::{NodeKind, NodeState, NodeStatus, VersionInfo};

    let state = NodeState {
        node_id: "f".into(),
        parent_id: "root".into(),
        name: "f".into(),
        kind: NodeKind::File as i32,
        status: NodeStatus::Live as i32,
        current: Some(VersionInfo {
            version_id: "current".into(),
            made_over: (0..100)
                .flat_map(|n| [format!("v{n}"), format!("v{n}")])
                .collect(),
            content: Some(Content::default()),
            ..VersionInfo::default()
        }),
        ..NodeState::default()
    };
    let (_, remote) = RemoteNode::from_state(&state).unwrap();
    assert_eq!(
        remote.made_over,
        (0..64).map(|n| format!("v{n}")).collect::<Vec<_>>()
    );
}

#[test]
fn ancestry_internal_rekey_preserves_the_value_and_leaves_the_old_synced_source() {
    for edited in [false, true] {
        let mut trees = ancestry_tree();
        if edited {
            ancestry_write(&mut trees, 2);
            ancestry_write(&mut trees, 3);
        }
        let before = trees.state().content_provenance["f"].clone();
        crate::commit::rekey(&mut trees, "f", "copy");
        let state = trees.state();
        let moved = &state.content_provenance["copy"];
        assert_eq!(moved.local, before.local);
        assert_eq!(moved.edit_bases, before.edit_bases);
        assert_eq!(moved.unversioned_bases, before.unversioned_bases);
        assert_eq!(moved.synced, None);
        assert_eq!(state.content_provenance["f"].synced, before.synced);
        assert_eq!(state.content_provenance["f"].local, None);
    }
}

#[test]
fn ancestry_an_explicit_local_copy_does_not_claim_the_originals_source() {
    let mut trees = ancestry_tree();
    let node = trees.l.get("f").unwrap().clone();
    trees.set_local_changed("copy".into(), node);
    assert_eq!(trees.source_version_id("copy"), None);
    assert!(trees.edit_bases("copy").is_empty());
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
    trees.settle(&Default::default());
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
    trees.settle(&Default::default());
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
    trees.settle(&Default::default());
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
    trees.settle(&Default::default());
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
        &Default::default(),
        0,
        0,
    );
    trees.settle(&Default::default());
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
    trees.settle(&Default::default());
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
        &Default::default(),
        0,
        0,
    );
    trees.settle(&Default::default());
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

#[test]
fn nodes_sharing_an_identity_or_a_name_key_are_found_in_id_order_as_they_come_and_go() {
    let identity = FileIdentity {
        ino: 7,
        birth_ns: Some(1),
    };
    let local = |name: &str| LocalNode {
        shape: Shape {
            parent: "root".into(),
            name: name.into(),
            kind: Kind::File,
            content: Some(Content {
                size: 1,
                manifest_digest: vec![1; 32],
                ..Content::default()
            }),
        },
        identity,
        parked: false,
        left_out_when_placed: false,
        content_from_server: false,
    };
    let mut trees = Trees::new("root");
    for (id, name) in [
        ("b", "Notes"),
        ("c", "notes"),
        ("a", "NOTES"),
        ("b", "Notes"),
    ] {
        trees.set_local(id.into(), local(name));
    }
    let held =
        |trees: &Trees| -> Vec<NodeId> { trees.locals_with_identity(&identity).cloned().collect() };
    assert_eq!(held(&trees), ["a", "b", "c"]);
    let reversed: Vec<NodeId> = trees
        .locals_with_identity(&identity)
        .rev()
        .cloned()
        .collect();
    assert_eq!(reversed, ["c", "b", "a"]);
    assert_eq!(
        trees.local_with_identity(&identity).map(String::as_str),
        Some("c")
    );
    assert_eq!(
        trees.local_child("root", "notes").map(String::as_str),
        Some("c")
    );
    assert_eq!(
        trees.local_child("root", "Notes").map(String::as_str),
        Some("b")
    );
    trees.remove_local("c");
    trees.remove_local("a");
    assert_eq!(held(&trees), ["b"]);
    assert_eq!(trees.local_child("root", "notes"), None);
    assert_eq!(
        trees.local_child("root", "Notes").map(String::as_str),
        Some("b")
    );
    trees.remove_local("b");
    assert!(held(&trees).is_empty());
    assert_eq!(trees.local_with_identity(&identity), None);
    assert_eq!(trees.local_child("root", "Notes"), None);
}

mod holders_against_a_rebuild {
    use super::*;
    use proptest::prelude::*;

    const NAMES: [&str; 4] = ["a", "A", "b", "B"];

    #[derive(Clone, Debug)]
    enum Op {
        Set(u8, usize, u64, bool),
        SetChanged(u8, usize, u64, bool),
        Remove(u8),
        Rekey(u8, u8, usize, u64),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u8..6, 0usize..4, 0u64..3, any::<bool>())
                .prop_map(|(i, n, k, p)| Op::Set(i, n, k, p)),
            (0u8..6, 0usize..4, 0u64..3, any::<bool>())
                .prop_map(|(i, n, k, p)| Op::SetChanged(i, n, k, p)),
            (0u8..6).prop_map(Op::Remove),
            (0u8..6, 0u8..6, 0usize..4, 0u64..3).prop_map(|(f, t, n, k)| Op::Rekey(f, t, n, k)),
        ]
    }

    fn ident(k: u64) -> FileIdentity {
        FileIdentity {
            ino: u128::from(k),
            birth_ns: Some(1),
        }
    }

    fn node(n: usize, k: u64, parked: bool) -> LocalNode {
        LocalNode {
            shape: Shape {
                parent: "root".into(),
                name: NAMES[n].into(),
                kind: Kind::File,
                content: Some(Content {
                    size: 1,
                    manifest_digest: vec![1; 32],
                    ..Content::default()
                }),
            },
            identity: ident(k),
            parked,
            left_out_when_placed: false,
            content_from_server: false,
        }
    }

    fn id(i: u8) -> NodeId {
        format!("n{i}")
    }

    fn check_index(index: &ByName, expected: &[(NodeId, &LocalNode)]) {
        let mut keys: BTreeMap<(NodeId, String), Vec<NodeId>> = BTreeMap::new();
        for (id, n) in expected {
            keys.entry((n.shape.parent.clone(), name_key(&n.shape.name)))
                .or_default()
                .push(id.clone());
        }
        for ids in keys.values_mut() {
            ids.sort();
        }
        assert_eq!(
            index.0.len(),
            keys.len(),
            "stale or missing keys: {:?} vs {:?}",
            index.0,
            keys
        );
        for (key, holders) in &index.0 {
            if let Holders::Many(set) = holders {
                assert!(set.len() >= 2, "Many with {} holders", set.len());
            }
            let got: Vec<NodeId> = holders.iter().cloned().collect();
            assert_eq!(Some(&got), keys.get(key));
            let back: Vec<NodeId> = holders.iter().rev().cloned().collect();
            let mut want = got.clone();
            want.reverse();
            assert_eq!(back, want);
        }
        for name in NAMES {
            let got: Vec<NodeId> = index.holders("root", name).cloned().collect();
            let want = keys
                .get(&("root".to_owned(), name_key(name)))
                .cloned()
                .unwrap_or_default();
            assert_eq!(got, want);
        }
        let children: Vec<NodeId> = index.children("root").cloned().collect();
        let want: Vec<NodeId> = keys.values().flatten().cloned().collect();
        assert_eq!(children, want);
    }

    fn check(trees: &Trees) {
        let all: Vec<(NodeId, &LocalNode)> = trees
            .l
            .iter()
            .filter(|(id, _)| id.as_str() != "root")
            .map(|(id, n)| (id.clone(), n))
            .collect();
        let placed: Vec<_> = all.iter().filter(|(_, n)| !n.parked).cloned().collect();
        let parked: Vec<_> = all.iter().filter(|(_, n)| n.parked).cloned().collect();
        check_index(&trees.l_names, &placed);
        check_index(&trees.l_parked, &parked);
        let mut live = 0;
        for k in 0..3 {
            let mut want: Vec<NodeId> = all
                .iter()
                .filter(|(_, n)| n.identity == ident(k))
                .map(|(id, _)| id.clone())
                .collect();
            want.sort();
            let got: Vec<NodeId> = trees.locals_with_identity(&ident(k)).cloned().collect();
            assert_eq!(got, want);
            let back: Vec<NodeId> = trees
                .locals_with_identity(&ident(k))
                .rev()
                .cloned()
                .collect();
            let mut rev = want.clone();
            rev.reverse();
            assert_eq!(back, rev);
            assert_eq!(trees.local_with_identity(&ident(k)), want.last());
            live += usize::from(!want.is_empty());
        }
        let root_identities = trees
            .l
            .iter()
            .filter(|(id, _)| id.as_str() == "root")
            .count();
        assert!(
            trees.l_identities.len() <= live + root_identities,
            "stale identities: {:?}",
            trees.l_identities
        );
        for holders in trees.l_identities.values() {
            if let Holders::Many(set) = holders {
                assert!(set.len() >= 2);
            }
        }
    }

    proptest! {
        #![proptest_config(koidrive_test_support::battery_config(
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/classify/tests.proptest-regressions"),
            2_048,
        ))]
        #[test]
        fn holders_match_a_rebuild_from_l(ops in proptest::collection::vec(op(), 1..40)) {
            let mut trees = Trees::new("root");
            for op in ops {
                match op {
                    Op::Set(i, n, k, p) => trees.set_local(id(i), node(n, k, p)),
                    Op::SetChanged(i, n, k, p) => trees.set_local_changed(id(i), node(n, k, p)),
                    Op::Remove(i) => trees.remove_local(&id(i)),
                    Op::Rekey(f, t, n, k) => {
                        if f != t && trees.l.get(&id(f)).is_some() {
                            trees.rekey_local(&id(f), &id(t), node(n, k, false));
                        }
                    }
                }
                check(&trees);
            }
        }
    }
}

#[test]
fn holders_take_no_more_room_than_a_set() {
    assert_eq!(
        std::mem::size_of::<Holders>(),
        std::mem::size_of::<BTreeSet<NodeId>>()
    );
}

fn kept_local(parent: &str, name: &str, kind: Kind, ino: u128) -> LocalNode {
    LocalNode {
        shape: Shape {
            parent: parent.into(),
            name: name.into(),
            kind,
            content: None,
        },
        identity: FileIdentity {
            ino,
            birth_ns: Some(0),
        },
        parked: false,
        left_out_when_placed: false,
        content_from_server: false,
    }
}

fn with_a_kept_env() -> Trees {
    let mut trees = Trees::new("root");
    trees.set_local("app".into(), kept_local("root", "app", Kind::Dir, 1));
    trees.kept_entries.insert(
        "app".into(),
        KeptEntries {
            names: [".env".into()].into(),
            moved: BTreeSet::new(),
            listed: true,
        },
    );
    trees.set_local("env".into(), kept_local("app", ".env", Kind::File, 2));
    trees
}

#[test]
fn a_node_at_a_reserved_name_stays_kept_under_a_new_id_and_keeps_the_name_reserved() {
    let mut trees = with_a_kept_env();
    assert!(trees.kept_nodes.contains("env"));

    trees.rekey_local("env", "env-2", kept_local("app", ".env", Kind::File, 2));

    assert!(trees.ignored("env-2"));
    assert_eq!(*trees.kept_nodes, BTreeSet::from(["env-2".to_owned()]));
    assert!(
        trees
            .kept_entries
            .get("app")
            .unwrap()
            .names
            .contains(".env")
    );
}

#[test]
fn a_kept_node_moved_off_its_reserved_name_stays_kept_under_a_new_id() {
    let mut trees = with_a_kept_env();
    trees.set_local("other".into(), kept_local("root", "other", Kind::Dir, 3));
    trees.set_local("env".into(), kept_local("other", "env", Kind::File, 2));

    trees.rekey_local("env", "env-2", kept_local("other", "env", Kind::File, 2));

    assert!(trees.ignored("env-2"));
    assert_eq!(*trees.kept_nodes, BTreeSet::from(["env-2".to_owned()]));
}

#[test]
fn a_kept_node_stays_kept_wherever_it_moves_until_l_drops_it() {
    let mut trees = with_a_kept_env();
    trees.set_local("other".into(), kept_local("root", "other", Kind::Dir, 3));

    trees.set_local(
        "env".into(),
        kept_local("other", ".env.local", Kind::File, 2),
    );
    trees.remove_local("app");

    assert!(trees.ignored("env"));
    assert_eq!(trees.kept_entry_paths(), [vec!["other", ".env.local"]]);
    trees.remove_local("env");
    assert!(trees.kept_nodes.is_empty());
    assert!(trees.kept_entry_paths().is_empty());
}

#[test]
fn a_synced_node_moved_onto_a_reserved_name_stays_synced() {
    let mut trees = with_a_kept_env();
    trees.set_remote("notes".into(), remote(dir("root", "notes")));
    trees.set_local("notes".into(), kept_local("root", "notes", Kind::Dir, 4));

    trees.remove_local("env");
    trees.kept_entries.insert(
        "app".into(),
        KeptEntries {
            names: ["notes".into()].into(),
            moved: BTreeSet::new(),
            listed: true,
        },
    );
    trees.set_local("notes".into(), kept_local("app", "notes", Kind::Dir, 4));

    assert!(!trees.ignored("notes"));
    assert!(!trees.kept_nodes.contains("notes"));
}
