// SPDX-License-Identifier: Apache-2.0

/// The server lost the commit that deleted folder d with file a in it, and a bootstrap lists both
/// live as they were deleted, at seq 1. The delete is sent again, not taken for a remote create.
#[test]
fn a_delete_the_server_lost_is_sent_again() {
    let rows = [synced("d", dir(ROOT, "d")), synced("a", file("d", "a", 1))];
    let (mut commits, mut trees, mut ids, sent) = deleted_at_5_then_lost(&rows);
    assert_eq!(sent, [delete("d")]);
    let trashed = trees.trashed_here();
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    trees.set_remote("a".into(), remote(file("d", "a", 1)));
    trees.forget_lost(
        trashed,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(
        (trees.class("d"), trees.class("a")),
        (Class::Local, Class::Local)
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("d")]);
}

/// Of three files this device deleted at seq 5, a bootstrap from a server restored from seq 2 lists
/// `same` as it was deleted, `edited` with other content, and `restored` as it was deleted but
/// restored after the loss at seq 3, a seq below the lost delete's that the lost commits had. Only
/// `same` is deleted again; the other two are downloaded.
#[test]
fn a_delete_the_server_lost_is_sent_again_only_for_what_it_saw() {
    let rows = [
        synced("same", file(ROOT, "same", 1)),
        synced("edited", file(ROOT, "edited", 1)),
        synced("restored", file(ROOT, "restored", 1)),
    ];
    let (mut commits, mut trees, mut ids, _) = deleted_at_5_then_lost(&rows);
    let trashed = trees.trashed_here();
    assert_eq!(trashed.len(), 3);
    trees.set_remote("same".into(), remote(file(ROOT, "same", 1)));
    let edited = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        ..remote(file(ROOT, "edited", 2))
    };
    trees.set_remote("edited".into(), edited);
    let restored = RemoteNode {
        meta_rev: 3,
        meta_edit_rev: 3,
        ..remote(file(ROOT, "restored", 1))
    };
    trees.set_remote("restored".into(), restored);
    trees.forget_lost(
        trashed,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let classes = ["same", "edited", "restored"].map(|id| (id, trees.class(id)));
    assert_eq!(
        classes,
        [
            ("same", Class::Local),
            ("edited", Class::Remote),
            ("restored", Class::Remote)
        ]
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("same")]);
}

/// A bootstrap from the server that lost the edit lists a with content 1 of seq 1, which the server
/// held before the loss. The edit is sent again as an edit, not kept beside a as a conflict copy.
#[test]
fn an_edit_the_server_lost_is_sent_again() {
    let (mut commits, mut trees, mut ids) = edited_at_5_then_lost();
    let edited = trees.edited_here();
    assert_eq!(edited, [("a".to_string(), 5)]);
    trees.set_remote("a".into(), remote(file(ROOT, "a", 1)));
    trees.forget_lost(
        Vec::new(),
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(trees.class("a"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 2)]);
}

/// As above, but another device wrote content 3 into a after the loss, at seq 3, a seq the lost
/// commits had. The edit is not sent over it: a stays a concurrent change, as before.
#[test]
fn an_edit_the_server_lost_is_not_sent_again_over_a_later_edit() {
    let (commits, mut trees, _) = edited_at_5_then_lost();
    let edited = trees.edited_here();
    let later = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        ..remote(file(ROOT, "a", 3))
    };
    trees.set_remote("a".into(), later);
    trees.forget_lost(
        Vec::new(),
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(trees.class("a"), Class::Concurrent);
}

/// As above, but another device that never saw the edit trashed a after the loss, at seq 3. An
/// edit beats a delete (R8), so the edit is sent again and revives a.
#[test]
fn an_edit_the_server_lost_is_sent_again_over_a_later_delete() {
    let (mut commits, mut trees, mut ids) = edited_at_5_then_lost();
    let edited = trees.edited_here();
    let deleted = RemoteNode {
        meta_rev: 3,
        meta_edit_rev: 3,
        ..trashed(file(ROOT, "a", 1))
    };
    trees.set_remote("a".into(), deleted);
    trees.forget_lost(
        Vec::new(),
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 2)]);
}

#[test]
fn a_local_edit_on_a_reverted_file_is_kept_on_disk_and_uploaded() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    trees.set_local("a".into(), local(file(ROOT, "x", 2), 1));
    let mut effects = Effects::default();
    let record = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert!(
        matches!(record.effect, Effect::Move { .. }),
        "{:?}",
        record.effect
    );
    effects
        .done(&mut trees, commits.in_flight(), &record.effect_id, None)
        .unwrap();
    trees.settle();
    assert_eq!(
        trees.local().get("a").unwrap().shape,
        file(ROOT, "x (conflict)", 2)
    );
    assert!(trees.reverted().is_empty());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 2)]);
}

/// Folder a is merged into b, which L holds elsewhere, so a is reverted and what is created in it
/// goes into b. The user then renames a, which ends the revert: a is created on its own, and a
/// file created in it afterwards goes into a, not into b.
#[test]
fn a_merged_folder_the_user_renames_takes_what_is_created_in_it() {
    let mut trees = trees(&[
        ("a", Some(dir(ROOT, "d")), None, None),
        synced("b", dir(ROOT, "e")),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&first)[..], [op::Op::CreateDir(c)] if c.node_id == "a"));
    trees.set_remote("b".into(), remote(dir(ROOT, "d")));
    let merged = OpResult {
        node_id: "b".into(),
        ..outcome(op_result::Code::MergedInto)
    };
    acknowledge(&mut commits, &mut trees, vec![merged]);
    assert!(trees.reverted().contains("a"));
    trees.set_local("a".into(), local(dir(ROOT, "user"), 1));
    trees.settle();
    let next = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&next)[..], [op::Op::CreateDir(c)] if c.node_id == "a" && c.name == "user")
    );
    trees.set_remote("a".into(), remote(dir(ROOT, "user")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.set_local("g".into(), local(file("a", "new", 2), 9));
    trees.settle();
    let child = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&child)[..], [op::Op::CreateFile(c)] if c.parent_id == "a"),
        "{:?}",
        ops(&child)
    );
}

/// The server renames a create or move of n to "é (conflict)". Before the result arrives, a scan
/// reports n under the decomposed spelling of the name the commit sent. That is the same name, not
/// a user move, so n stays reverted and the refused name is not sent again.
#[test]
fn a_scan_that_respells_the_sent_name_keeps_a_renamed_result_reverted() {
    for is_create in [true, false] {
        let before = (!is_create).then(|| dir(ROOT, "old"));
        let mut trees = trees(&[(
            "n",
            Some(dir(ROOT, "é")),
            before.clone(),
            before.map(remote),
        )]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        trees.set_local("n".into(), local(dir(ROOT, "e\u{301}"), 1));
        trees.set_remote("n".into(), remote(dir(ROOT, "é (conflict)")));
        let renamed = OpResult {
            name: "é (conflict)".into(),
            ..outcome(op_result::Code::AppliedRenamed)
        };
        acknowledge(&mut commits, &mut trees, vec![renamed]);
        assert!(trees.reverted().contains("n"), "create={is_create}");
        let retry = build(&mut commits, &mut trees, &mut ids);
        assert!(retry.is_none(), "create={is_create}, retry={retry:?}");
    }
}

#[test]
fn a_reverted_file_deleted_remotely_keeps_its_unuploaded_edit_as_a_new_file() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    trees.set_local("a".into(), local(file(ROOT, "x", 2), 1));
    trees.set_remote("a".into(), trashed(file(ROOT, "x (conflict)", 1)));
    trees.settle();
    assert!(trees.reverted().contains("a"));
    let mut effects = Effects::default();
    assert_eq!(
        effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first(),
        None
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let [op::Op::CreateFile(create)] = &ops(&commit)[..] else {
        panic!("{:?}", ops(&commit));
    };
    assert_eq!(
        (create.parent_id.as_str(), create.name.as_str()),
        (ROOT, "x")
    );
    assert_eq!(create.content, Some(content(2)));
    assert!(trees.local().get("a").is_none());
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.set_remote(create.node_id.clone(), remote(file(ROOT, "x", 2)));
    trees.settle();
    assert_eq!(trees.work(), vec![]);
    assert!(trees.reverted().is_empty());
}

#[test]
fn a_renamed_file_the_user_deletes_before_it_moves_is_deleted() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    trees.remove_local("a");
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [op::Op::Delete(proto::Delete {
            node_id: "a".into(),
            requires: None
        })]
    );
}

#[test]
fn two_reverted_files_that_block_each_other_are_placed_through_parking() {
    let mut trees = trees(&[
        ("a", Some(file(ROOT, "x", 1)), None, None),
        ("b", Some(file(ROOT, "y", 2)), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let renamed = |name: &str| OpResult {
        name: name.into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(
        &mut commits,
        &mut trees,
        vec![renamed("x (conflict)"), renamed("y (conflict)")],
    );
    trees.set_remote("a".into(), remote(file(ROOT, "y", 1)));
    trees.set_remote("b".into(), remote(file(ROOT, "x", 2)));
    trees.settle();
    let mut effects = Effects::default();
    for _ in 0..8 {
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        effects
            .done(&mut trees, commits.in_flight(), &record.effect_id, None)
            .unwrap();
        trees.settle();
    }
    assert_eq!(trees.local().get("a").unwrap().shape, file(ROOT, "y", 1));
    assert_eq!(trees.local().get("b").unwrap().shape, file(ROOT, "x", 2));
    assert!(trees.reverted().is_empty());
}

#[test]
fn a_folder_deleted_around_a_parked_node_is_brought_back_for_it() {
    for name in ["f", "F"] {
        let mut trees = trees(&[
            (
                "d",
                None,
                Some(dir(ROOT, "d")),
                Some(remote(dir(ROOT, "d"))),
            ),
            (
                "f",
                Some(file("d", "F", 1)),
                Some(file("d", "F", 1)),
                Some(remote(file("d", name, 1))),
            ),
        ]);
        trees.set_local(
            "f".into(),
            LocalNode {
                parked: true,
                ..local(file("d", "F", 1), 2)
            },
        );
        trees.settle();
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None, "{name}");
        assert!(trees.reverted().contains("d"), "{name}");
        let mut effects = Effects::default();
        for ino in 10..18 {
            trees.settle();
            let Some(record) = effects
                .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
                .first()
                .cloned()
            else {
                break;
            };
            let identity = FileIdentity {
                dev: 1,
                ino,
                birth_ns: Some(0),
            };
            effects
                .done(
                    &mut trees,
                    commits.in_flight(),
                    &record.effect_id,
                    Some(identity),
                )
                .unwrap();
        }
        trees.settle();
        assert_eq!(trees.work(), vec![], "{name}");
        assert_eq!(trees.local().get("d").unwrap().shape, dir(ROOT, "d"));
        let f = trees.local().get("f").unwrap();
        assert_eq!((&f.shape, f.parked), (&file("d", name, 1), false));
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None, "{name}");
    }
}

#[test]
fn a_parked_file_rekeyed_for_its_edit_is_moved_out_from_where_it_was_parked() {
    let mut trees = trees(&[
        (
            "a",
            Some(file(ROOT, "x", 2)),
            Some(file(ROOT, "x (conflict)", 1)),
            Some(remote(file(ROOT, "y", 1))),
        ),
        (
            "c",
            Some(file(ROOT, "y", 4)),
            Some(file(ROOT, "y (conflict)", 4)),
            Some(remote(file(ROOT, "x", 4))),
        ),
    ]);
    trees.revert("a".into());
    trees.revert("c".into());
    let (mut commits, mut ids, mut effects) =
        (Commits::default(), IdGen::new(1), Effects::default());
    let parking = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert!(
        matches!(&parking.effect, Effect::Move { to: Location::Parked(id), .. } if id == "a"),
        "{parking:?}"
    );
    effects
        .done(&mut trees, commits.in_flight(), &parking.effect_id, None)
        .unwrap();
    trees.set_remote("a".into(), trashed(file(ROOT, "y", 1)));
    trees.set_remote("c".into(), trashed(file(ROOT, "x", 4)));
    trees.set_local("b".into(), local(file(ROOT, "x", 3), 3));
    trees.settle();
    let remove = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert_eq!(remove.node, "c");
    effects
        .done(&mut trees, commits.in_flight(), &remove.effect_id, None)
        .unwrap();
    trees.settle();
    assert!(
        effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .is_empty()
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), 2);
    let rekeyed = trees
        .local()
        .iter()
        .find(|(_, node)| node.parked)
        .unwrap()
        .0
        .clone();
    assert_ne!(rekeyed, "a");
    assert_eq!(trees.parked_name(&rekeyed), "a");
    trees.set_remote(rekeyed.clone(), remote(file(ROOT, "x", 2)));
    trees.set_remote("b".into(), remote(file(ROOT, "x (new conflict)", 3)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![
            outcome(op_result::Code::Applied),
            OpResult {
                name: "x (new conflict)".into(),
                ..outcome(op_result::Code::AppliedRenamed)
            },
        ],
    );
    let mut unparked = false;
    for ino in 10..16 {
        trees.settle();
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        if record.node == rekeyed {
            let Effect::Move { from, .. } = &record.effect else {
                panic!("{record:?}");
            };
            assert_eq!(from, &Location::Parked("a".into()));
            unparked = true;
        }
        let identity = FileIdentity {
            dev: 1,
            ino,
            birth_ns: Some(0),
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
    }
    assert!(unparked);
    trees.settle();
    assert_eq!(trees.work(), vec![]);
    let node = trees.local().get(&rekeyed).unwrap();
    assert_eq!((&node.shape, node.parked), (&file(ROOT, "x", 2), false));
    assert_eq!(trees.parked_name(&rekeyed), rekeyed);
    let b = &trees.local().get("b").unwrap().shape;
    assert_eq!(b, &file(ROOT, "x (new conflict)", 3));
}

/// The server renamed the create of x to y, and the file is reverted until the effects move it
/// there. Then the server loses the create. Once S forgets the file, it is no longer reverted, so
/// it is created again under its own ID at x, instead of as a new node beside the one another
/// device creates again (section 4.4).
#[test]
fn a_reverted_file_the_server_lost_is_created_again_under_its_id() {
    let renamed = file(ROOT, "y", 1);
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "x", 1)),
        Some(renamed.clone()),
        Some(remote(renamed)),
    )]);
    trees.revert("h".into());
    trees.remove_remote("h");
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
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "h" && c.name == "x"),
        "{:?}",
        ops(&commit)
    );
}

#[test]
fn a_reverted_file_whose_s_update_waits_is_created_again_under_its_id_once_the_server_lost_it() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "y", 9)),
        Some(file(ROOT, "y", 9)),
        Some(trashed(file(ROOT, "y", 9))),
    )]);
    trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("n".into(), remote(file(ROOT, "y", 1)));
    let renamed = OpResult {
        name: "y".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    assert!(trees.waiting().contains_key("n"), "S still has h at y");
    assert!(trees.reverted().contains("n"));
    trees.remove_remote("n");
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
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n" && c.name == "x"),
        "{:?}",
        ops(&commit)
    );
}

#[test]
fn a_file_whose_s_update_waits_is_removed_or_uploaded_anew_after_a_remote_delete() {
    for (edit, parked) in [(None, false), (None, true), (Some(2), false)] {
        let mut trees = trees(&[synced("h", file(ROOT, "x", 9))]);
        trees.set_remote("h".into(), remote(file(ROOT, "y", 9)));
        trees.set_local(
            "h".into(),
            LocalNode {
                parked: true,
                ..local(file(ROOT, "x", 9), 1)
            },
        );
        trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
        trees.settle();
        let (mut commits, mut ids, mut effects) =
            (Commits::default(), IdGen::new(1), Effects::default());
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
        trees.set_remote("n".into(), remote(file(ROOT, "x", 1)));
        acknowledge(
            &mut commits,
            &mut trees,
            vec![outcome(op_result::Code::Applied)],
        );
        assert!(trees.waiting().contains_key("n"), "S still has h at x");
        if let Some(digest) = edit {
            trees.set_local("n".into(), local(file(ROOT, "x", digest), 3));
        }
        if parked {
            trees.set_local(
                "n".into(),
                LocalNode {
                    parked: true,
                    ..local(file(ROOT, "x", 1), 2)
                },
            );
            trees.set_local("b".into(), local(file(ROOT, "x", 5), 4));
        }
        trees.set_remote("n".into(), trashed(file(ROOT, "x", 1)));
        trees.settle();
        let mut removed = false;
        for _ in 0..4 {
            let Some(record) = effects
                .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
                .first()
                .cloned()
            else {
                break;
            };
            if record.node == "h" {
                effects.failed(&mut trees, &record.effect_id).unwrap();
                continue;
            }
            assert_eq!(record.node, "n", "{record:?}");
            removed = matches!(record.effect, Effect::RemoveFile { .. });
            effects
                .done(&mut trees, commits.in_flight(), &record.effect_id, None)
                .unwrap();
            trees.settle();
        }
        let commit = build(&mut commits, &mut trees, &mut ids);
        match edit {
            None => {
                assert!(
                    removed,
                    "a file with no edit follows the delete, parked={parked}"
                );
                assert!(trees.local().get("n").is_none());
                let ops = commit.map(|commit| ops(&commit)).unwrap_or_default();
                assert!(
                    ops.iter()
                        .all(|op| matches!(op, op::Op::CreateFile(c) if c.node_id == "b")),
                    "{ops:?}"
                );
            }
            Some(digest) => {
                assert!(!removed, "an edit is never removed");
                let ops = ops(&commit.expect("the edit is uploaded"));
                assert!(
                    matches!(&ops[..], [op::Op::CreateFile(c)]
                        if c.node_id != "n" && c.content == Some(content(digest))),
                    "{ops:?}"
                );
            }
        }
    }
}

#[test]
fn a_parked_reverted_folder_the_server_lacks_is_removed_even_when_its_name_is_taken() {
    let mut trees = trees(&[synced("h", dir(ROOT, "x"))]);
    trees.set_local(
        "n".into(),
        LocalNode {
            parked: true,
            ..local(dir(ROOT, "x"), 2)
        },
    );
    trees.revert("n".into());
    trees.settle();
    let mut effects = Effects::default();
    let record = effects
        .plan(&trees, None, &mut IdGen::new(1), NOW, BatchLimit::ONE)
        .first()
        .cloned();
    let record = record.expect("an effect for the parked folder");
    assert!(
        matches!(record.effect, Effect::RemoveDir { .. }),
        "{record:?}"
    );
    effects
        .done(&mut trees, None, &record.effect_id, None)
        .unwrap();
    trees.settle();
    assert_eq!(trees.work(), vec![]);
    assert!(trees.local().get("n").is_none());
    assert!(trees.reverted().is_empty());
}

#[test]
fn a_move_another_device_made_after_a_loss_is_taken() {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "x", 1)),
        Some(file(ROOT, "x", 1)),
        Some(remote(file(ROOT, "x", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let (trashed, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    trees.set_remote("n".into(), remote_at(file(ROOT, "y", 1), 3));
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        moved,
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(trees.class("n"), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_move_of_this_device_the_server_lost_is_sent_again() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    move_n_to_y_at(2, &mut trees, &mut commits, &mut ids);
    let n = remote(file(ROOT, "x", 1));
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert_eq!(sent, [mv("n", ROOT, "y")]);
}

#[test]
fn a_lost_move_is_not_sent_over_a_move_another_device_made_after_the_loss() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    move_n_to_y_at(2, &mut trees, &mut commits, &mut ids);
    let n = remote_at(file(ROOT, "z", 1), 3);
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert!(sent.is_empty());
    assert_eq!(trees.class("n"), Class::Remote);
}

#[test]
fn a_lost_move_is_sent_again_after_another_device_trashed_and_restored_the_node() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    move_n_to_y_at(2, &mut trees, &mut commits, &mut ids);
    let trashed_at_3 = RemoteNode {
        meta_rev: 3,
        ..trashed(file(ROOT, "y", 1))
    };
    trees.set_remote("n".into(), trashed_at_3);
    trees.set_remote("n".into(), remote_at(file(ROOT, "y", 1), 4));
    trees.settle();
    let n = remote(file(ROOT, "x", 1));
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert_eq!(sent, [mv("n", ROOT, "y")]);
}

#[test]
fn a_lost_move_is_sent_again_over_a_move_the_server_kept_from_before_it() {
    let (mut trees, mut commits, mut ids) = n_moved_to_z_at_7_after_a_commit_at_5();
    move_n_to_y_at(9, &mut trees, &mut commits, &mut ids);
    let n = remote_at(file(ROOT, "z", 1), 7);
    let sent = lose_with_n_at(n, 1, &mut trees, &mut commits, &mut ids);
    assert_eq!(commits.held_through(), 5);
    assert_eq!(sent, [mv("n", ROOT, "y")]);
}

#[test]
fn a_lost_move_is_sent_again_when_the_move_it_followed_was_lost_too() {
    let (mut trees, mut commits, mut ids) = n_moved_to_z_at_7_after_a_commit_at_5();
    move_n_to_y_at(9, &mut trees, &mut commits, &mut ids);
    let n = remote(file(ROOT, "x", 1));
    let sent = lose_with_n_at(n, 1, &mut trees, &mut commits, &mut ids);
    assert_eq!(commits.held_through(), 5);
    assert_eq!(sent, [mv("n", ROOT, "y")]);
}

#[test]
fn a_lost_first_move_is_sent_again_when_the_remote_move_it_followed_was_lost_too() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    take_remote_move("z", 7, &mut trees);
    move_n_to_y_at(9, &mut trees, &mut commits, &mut ids);
    let n = remote(file(ROOT, "x", 1));
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert_eq!(commits.held_through(), 0);
    assert_eq!(sent, [mv("n", ROOT, "y")]);
}

#[test]
fn a_lost_move_is_sent_again_over_any_position_the_server_kept_from_before_it() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    take_remote_move("w", 5, &mut trees);
    take_remote_move("z", 7, &mut trees);
    move_n_to_y_at(9, &mut trees, &mut commits, &mut ids);
    let n = remote_at(file(ROOT, "w", 1), 5);
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert_eq!(sent, [mv("n", ROOT, "y")]);
}

#[test]
fn a_lost_move_is_sent_again_over_a_position_the_server_kept_that_a_listing_skipped() {
    let (mut trees, mut commits, mut ids) = n_moved_to_z_at_7_after_a_commit_at_5();
    move_n_to_y_at(9, &mut trees, &mut commits, &mut ids);
    let n = remote_at(file(ROOT, "w", 1), 4);
    let sent = lose_with_n_at(n, 1, &mut trees, &mut commits, &mut ids);
    assert_eq!(sent, [mv("n", ROOT, "y")]);
}

#[test]
fn a_lost_move_is_not_sent_over_another_position_at_a_revision_this_device_saw() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    take_remote_move("z", 7, &mut trees);
    move_n_to_y_at(9, &mut trees, &mut commits, &mut ids);
    let n = remote_at(file(ROOT, "w", 1), 7);
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert!(sent.is_empty());
    assert_eq!(trees.class("n"), Class::Remote);
}

#[test]
fn a_lost_move_is_not_sent_once_another_device_moved_the_node_elsewhere() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    move_n_to_y_at(9, &mut trees, &mut commits, &mut ids);
    take_remote_move("z", 10, &mut trees);
    assert!(trees.moved_here().is_empty());
    let n = remote(file(ROOT, "x", 1));
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert!(sent.is_empty());
    assert_eq!(trees.class("n"), Class::Remote);
}

#[test]
fn a_move_is_no_longer_recorded_once_r_holds_its_position_at_an_older_revision() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    move_n_to_y_at(9, &mut trees, &mut commits, &mut ids);
    trees.set_remote("n".into(), remote_at(file(ROOT, "y", 1), 5));
    assert!(trees.moved_here().is_empty());
}

#[test]
fn a_lost_move_is_not_sent_over_a_position_this_device_never_saw() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    take_remote_move("z", 7, &mut trees);
    move_n_to_y_at(9, &mut trees, &mut commits, &mut ids);
    let n = remote_at(file(ROOT, "w", 1), 3);
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert!(sent.is_empty());
    assert_eq!(trees.class("n"), Class::Remote);
}

#[test]
fn a_full_set_of_slots_forgets_only_the_positions_below_its_oldest_seq() {
    let (mut trees, _, mut ids) = n_at_x();
    take_remote_move("w", 5, &mut trees);
    take_remote_move("v", 1500, &mut trees);
    take_remote_move("z", 1600, &mut trees);
    let mut commits = Commits::from_state(CommitsState {
        accepted: 999,
        slots: (1..=999).map(|slot| (slot, slot + 1000)).collect(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(past_revisions_of_n(&trees), [1, 5, 1500]);
    move_n_to_y_at(2000, &mut trees, &mut commits, &mut ids);
    assert_eq!(past_revisions_of_n(&trees), [5, 1500, 1600]);
}

#[test]
fn a_position_the_node_held_in_the_trash_is_not_kept_as_an_earlier_one() {
    let (mut trees, _, _) = n_at_x();
    trees.set_remote(
        "n".into(),
        RemoteNode {
            meta_rev: 5,
            ..trashed(file(ROOT, "w", 1))
        },
    );
    take_remote_move("z", 7, &mut trees);
    assert_eq!(past_revisions_of_n(&trees), [1]);
}

#[test]
fn a_lost_move_in_the_oldest_kept_commit_is_sent_again_over_the_position_it_replaced() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    move_n_to_y_at(2, &mut trees, &mut commits, &mut ids);
    let mut commits = Commits::from_state(CommitsState {
        accepted: 999,
        slots: std::iter::once((1, 2))
            .chain((2..=999).map(|slot| (slot, slot + 1000)))
            .collect(),
        ..Default::default()
    })
    .unwrap();
    trees.set_local("pad".into(), local(file(ROOT, "pad", 7), 99));
    trees.settle();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("pad".into(), remote_at(file(ROOT, "pad", 7), 3000));
    commits
        .received(result(3000, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 3000));
    trees.settle();
    let n = remote(file(ROOT, "x", 1));
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert_eq!(sent, [mv("n", ROOT, "y")]);
}

#[test]
fn a_lost_move_is_sent_again_after_another_device_trashed_and_restored_the_node_before_its_result()
{
    let (mut trees, mut commits, mut ids) = n_at_x();
    trees.set_local("n".into(), local(file(ROOT, "y", 1), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "y")]);
    trees.set_remote("n".into(), remote_at(file(ROOT, "y", 1), 2));
    let trashed_at_3 = RemoteNode {
        meta_rev: 3,
        ..trashed(file(ROOT, "y", 1))
    };
    trees.set_remote("n".into(), trashed_at_3);
    trees.set_remote("n".into(), remote_at(file(ROOT, "y", 1), 4));
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    let n = remote(file(ROOT, "x", 1));
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert_eq!(sent, [mv("n", ROOT, "y")]);
}

#[test]
fn a_lost_move_of_a_folder_keeps_a_move_another_device_made_of_its_child_after_the_loss() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "x")),
            Some(remote(dir(ROOT, "x"))),
        ),
        (
            "n",
            Some(file("p", "n", 1)),
            Some(file("p", "n", 1)),
            Some(remote(file("p", "n", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("p".into(), local(dir(ROOT, "y"), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("p", ROOT, "y")]);
    trees.set_remote("p".into(), remote_at(dir(ROOT, "y"), 2));
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    trees.set_remote("p".into(), remote(dir(ROOT, "x")));
    trees.set_remote("n".into(), remote_at(file(ROOT, "z", 1), 3));
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        moved,
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("p", ROOT, "y")]);
    assert_eq!(trees.class("n"), Class::Remote);
}

#[test]
fn a_move_recorded_by_the_store_upgrade_is_not_sent_over_a_move_made_after_its_commit() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    move_n_to_y_at(9, &mut trees, &mut commits, &mut ids);
    let mut state = trees.state();
    let upgraded = MovedHere {
        seq: 9,
        upgraded: true,
    };
    state.moved_here = BTreeMap::from([("n".into(), upgraded)]);
    state.past_positions.clear();
    trees.restore(state).unwrap();
    let n = remote_at(file(ROOT, "z", 1), 10);
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert!(sent.is_empty());
    assert_eq!(trees.class("n"), Class::Remote);
}

#[test]
fn a_lost_folder_move_keeps_its_childs_remote_move_when_its_old_name_is_held_in_s() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "x")),
            Some(remote(dir(ROOT, "x"))),
        ),
        (
            "q",
            Some(dir(ROOT, "z")),
            Some(dir(ROOT, "z")),
            Some(remote(dir(ROOT, "z"))),
        ),
        (
            "n",
            Some(file("p", "n", 1)),
            Some(file("p", "n", 1)),
            Some(remote(file("p", "n", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("p".into(), local(dir(ROOT, "y"), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("p", ROOT, "y")]);
    trees.set_remote("p".into(), remote_at(dir(ROOT, "y"), 2));
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.set_remote("q".into(), remote_at(dir(ROOT, "x"), 3));
    trees.set_local("q".into(), local(dir(ROOT, "x"), 2));
    trees.settle();
    assert_eq!(trees.synced().live("q").unwrap().name, "x");
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    trees.set_remote("p".into(), remote(dir(ROOT, "x")));
    trees.set_remote("q".into(), remote(dir(ROOT, "z")));
    trees.set_remote("n".into(), remote_at(file(ROOT, "w", 1), 4));
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        moved,
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert_eq!(sent, Some(vec![mv("p", ROOT, "y")]));
    assert_eq!(trees.class("n"), Class::Remote);
}

#[test]
fn r_keeps_the_newest_positions_a_node_held() {
    let (mut trees, _, _) = n_at_x();
    for rev in 2..=(PAST_POSITIONS_KEPT as u64 + 2) {
        trees.set_remote(
            "n".into(),
            remote_at(file(ROOT, &format!("p{rev}"), 1), rev),
        );
    }
    let kept = &trees.state().past_positions["n"];
    assert_eq!(kept.len(), PAST_POSITIONS_KEPT);
    assert_eq!(kept.first().map(|past| past.rev), Some(2));
}

#[test]
fn a_lost_move_is_sent_again_when_the_loss_also_took_a_delete_made_after_it() {
    let (mut trees, mut commits, mut ids) = n_at_x();
    trees.set_local("n".into(), local(file(ROOT, "y", 1), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "y")]);
    trees.set_remote("n".into(), remote_at(file(ROOT, "y", 1), 2));
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    let trashed_at_3 = RemoteNode {
        meta_rev: 3,
        ..trashed(file(ROOT, "y", 1))
    };
    trees.set_remote("n".into(), trashed_at_3);
    trees.settle();
    let n = remote(file(ROOT, "x", 1));
    let sent = lose_with_n_at(n, 0, &mut trees, &mut commits, &mut ids);
    assert_eq!(sent, [mv("n", ROOT, "y")]);
}

#[test]
fn a_delete_the_server_lost_is_sent_again_for_a_record_it_saw_above_the_bound() {
    let seen = |shape: Shape| RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        meta_rev: 3,
        version_id: "v3".into(),
        ..remote(shape)
    };
    let row = |id: &'static str, r: RemoteNode| (id, Some(r.shape.clone()), Some(r.shape.clone()), Some(r));
    let rows = [
        row("dir", remote_at(dir(ROOT, "dir"), 3)),
        row("same", seen(file(ROOT, "same", 1))),
        row("restored", seen(file(ROOT, "restored", 1))),
        row("marked", seen(file(ROOT, "marked", 1))),
    ];
    let (mut commits, mut trees, mut ids, _) = deleted_at_5_then_lost(&rows);
    for id in ["same", "restored", "marked"] {
        let deleted = RemoteNode {
            meta_rev: 5,
            trash_group: "5:0".into(),
            trashed: true,
            ..seen(file(ROOT, id, 1))
        };
        trees.set_remote(id.into(), deleted);
    }
    let trashed = trees.trashed_here();
    assert_eq!(trashed.len(), 4);
    trees.set_listed_remote("dir".into(), remote_at(dir(ROOT, "dir"), 3));
    trees.set_listed_remote("same".into(), seen(file(ROOT, "same", 1)));
    let restored = RemoteNode {
        meta_rev: 7,
        ..seen(file(ROOT, "restored", 1))
    };
    trees.set_listed_remote("restored".into(), restored);
    let marked = RemoteNode {
        edit_rev: 6,
        ..seen(file(ROOT, "marked", 1))
    };
    trees.set_listed_remote("marked".into(), marked);
    trees.forget_lost(
        trashed,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let classes = ["dir", "same", "restored", "marked"].map(|id| (id, trees.class(id)));
    assert_eq!(
        classes,
        [
            ("dir", Class::Local),
            ("same", Class::Local),
            ("restored", Class::Remote),
            ("marked", Class::Remote)
        ]
    );
    let mut sent = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    sent.sort_by_key(|op| format!("{op:?}"));
    assert_eq!(sent, [delete("dir"), delete("same")]);
}

#[test]
fn a_file_this_device_made_follows_a_delete_after_another_device_edited_it_past_a_loss() {
    let made = RemoteNode {
        meta_rev: 2,
        content_rev: 2,
        ..remote(file(ROOT, "a", 1))
    };
    let held = Some(file(ROOT, "a", 1));
    let mut trees = trees(&[("a", held.clone(), held, Some(made))]);
    let mut commits = Commits::from_state(CommitsState {
        accepted: 2,
        slots: [(1, 2), (2, 4)].into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let edited = RemoteNode {
        meta_rev: 2,
        content_rev: 3,
        ..remote(file(ROOT, "a", 2))
    };
    trees.set_listed_remote("a".into(), edited);
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        synced,
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let deleted = RemoteNode {
        meta_rev: 4,
        content_rev: 3,
        ..trashed(file(ROOT, "a", 2))
    };
    trees.set_remote("a".into(), deleted);
    trees.settle();
    assert_eq!(trees.class("a"), Class::Remote);
    assert!(build(&mut commits, &mut trees, &mut IdGen::new(1)).is_none());
}

#[test]
fn a_file_another_device_edited_before_a_loss_follows_a_delete_made_after_it() {
    let edited = RemoteNode {
        content_rev: 3,
        version_id: "v2".into(),
        ..remote(file(ROOT, "a", 2))
    };
    let held = Some(file(ROOT, "a", 1));
    let mut trees = trees(&[("a", held.clone(), held, Some(edited.clone()))]);
    let untaken = trees.untaken_changes(|seq| seq == 4);
    assert_eq!(
        untaken,
        [(
            "a".to_owned(),
            Untaken::Edit {
                moved_in_lost_commit: false
            }
        )]
    );
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        untaken,
        Default::default(),
        0,
        0,
    );
    trees.settle();
    assert_eq!(trees.class("a"), Class::Remote);
    let deleted = RemoteNode {
        meta_rev: 4,
        trashed: true,
        trash_group: "4:0".into(),
        ..edited
    };
    trees.set_remote("a".into(), deleted);
    trees.settle();
    assert_eq!(trees.class("a"), Class::Remote);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

fn an_untaken_edit_then(listed: RemoteNode) -> (Trees, Option<proto::Commit>) {
    let edited = RemoteNode {
        content_rev: 3,
        version_id: "v2".into(),
        ..remote(file(ROOT, "a", 2))
    };
    let held = Some(file(ROOT, "a", 1));
    let mut trees = trees(&[
        (
            "d",
            Some(dir(ROOT, "d")),
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        ("a", held.clone(), held, Some(edited)),
    ]);
    let untaken = trees.untaken_changes(|seq| seq == 4);
    trees.set_listed_remote("a".into(), listed.clone());
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        untaken,
        Default::default(),
        0,
        0,
    );
    trees.settle();
    let deleted = RemoteNode {
        meta_rev: 9,
        trashed: true,
        trash_group: "9:0".into(),
        ..listed
    };
    trees.set_remote("a".into(), deleted);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids);
    (trees, commit)
}

#[test]
fn an_untaken_edit_edited_again_after_the_loss_follows_a_later_delete() {
    let (trees, commit) = an_untaken_edit_then(RemoteNode {
        content_rev: 6,
        version_id: "v3".into(),
        ..remote(file(ROOT, "a", 3))
    });
    assert_eq!(trees.class("a"), Class::Remote);
    assert_eq!(commit, None);
}

#[test]
fn an_untaken_edit_moved_after_the_loss_follows_a_later_delete() {
    let (trees, commit) = an_untaken_edit_then(RemoteNode {
        content_rev: 3,
        meta_rev: 6,
        version_id: "v2".into(),
        ..remote(file("d", "a", 2))
    });
    assert_eq!(trees.class("a"), Class::Remote);
    assert_eq!(commit, None);
}

#[test]
fn an_untaken_edit_below_a_lost_folder_follows_a_later_delete() {
    let edited = RemoteNode {
        content_rev: 3,
        version_id: "v2".into(),
        ..remote(file("f", "a", 2))
    };
    let held = Some(file("f", "a", 1));
    let mut trees = trees(&[
        (
            "f",
            Some(dir(ROOT, "g")),
            Some(dir(ROOT, "g")),
            Some(remote_at(dir(ROOT, "g"), 4)),
        ),
        ("a", held.clone(), held, Some(edited.clone())),
    ]);
    let untaken = trees.untaken_changes(|seq| seq == 4);
    trees.set_listed_remote("a".into(), remote(file(ROOT, "a", 2)));
    trees.remove_remote("f");
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        untaken,
        Default::default(),
        0,
        0,
    );
    trees.settle();
    assert!(trees.unsent_edits().is_empty());
    let deleted = RemoteNode {
        meta_rev: 5,
        trashed: true,
        trash_group: "5:0".into(),
        ..edited
    };
    trees.set_remote("a".into(), deleted);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateDir(c)] if c.node_id == "f" && c.name == "g"),
        "{:?}",
        ops(&commit)
    );
    trees.set_remote("f".into(), remote_at(dir(ROOT, "g"), 6));
    commits
        .received(result(6, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 6));
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.class("a"), Class::Remote);
}

#[test]
fn an_untaken_edit_renamed_in_a_lost_commit_is_renamed_again() {
    let edited = RemoteNode {
        content_rev: 3,
        version_id: "v2".into(),
        ..remote_at(file(ROOT, "b", 2), 4)
    };
    let held = Some(file(ROOT, "b", 1));
    let mut trees = trees(&[("a", held.clone(), held, Some(edited))]);
    let untaken = trees.untaken_changes(|seq| seq == 4);
    assert_eq!(
        untaken,
        [(
            "a".to_owned(),
            Untaken::Edit {
                moved_in_lost_commit: true
            }
        )]
    );
    let listed = RemoteNode {
        content_rev: 3,
        version_id: "v2".into(),
        ..remote(file(ROOT, "a", 2))
    };
    trees.set_listed_remote("a".into(), listed);
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        untaken,
        Default::default(),
        3,
        0,
    );
    trees.settle();
    assert!(trees.unsent_edits().is_empty());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("a", ROOT, "b")]);
}

#[test]
fn an_untaken_edit_another_device_moved_before_the_loss_stays_where_it_moved() {
    let moved = RemoteNode {
        content_rev: 3,
        version_id: "v2".into(),
        ..remote_at(file("d", "a", 2), 2)
    };
    let held = Some(file(ROOT, "a", 1));
    let mut trees = trees(&[
        (
            "d",
            Some(dir(ROOT, "d")),
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        ("a", held.clone(), held, Some(moved.clone())),
    ]);
    let untaken = trees.untaken_changes(|seq| seq == 4);
    assert_eq!(
        untaken,
        [(
            "a".to_owned(),
            Untaken::Edit {
                moved_in_lost_commit: false
            }
        )]
    );
    trees.set_listed_remote("a".into(), moved);
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        untaken,
        Default::default(),
        3,
        0,
    );
    trees.settle();
    assert_eq!(trees.class("a"), Class::Remote);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn content_l_went_back_to_while_an_acknowledged_edit_waits_is_sent_after_a_loss() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        (
            "n",
            Some(file(ROOT, "old", 0)),
            Some(file(ROOT, "old", 0)),
            Some(remote(file(ROOT, "old", 0))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.set_local("pad".into(), local(file(ROOT, "pad", 7), 99));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let pad = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        ..remote(file(ROOT, "pad", 7))
    };
    trees.set_remote("pad".into(), pad);
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
    let moved = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        meta_rev: 5,
        ..remote(file(ROOT, "x", 1))
    };
    trees.set_remote("n".into(), moved);
    let applied = vec![outcome(op_result::Code::Applied); 2];
    commits.received(result(5, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert!(trees.waiting().contains_key("n"));
    trees.set_local("n".into(), local(file(ROOT, "x", 0), 3));
    trees.settle();
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let theirs = RemoteNode {
        content_rev: 6,
        edit_rev: 6,
        ..remote(file(ROOT, "old", 7))
    };
    trees.set_remote("n".into(), theirs);
    trees.forget_lost(
        trashed,
        edited,
        synced,
        Vec::new(),
        untaken,
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        ops(&commit).iter().any(|op| matches!(
            op,
            op::Op::CreateFile(proto::CreateFile { content: Some(content), .. })
                | op::Op::UpdateContent(proto::UpdateContent { content: Some(content), .. })
                if content.manifest_digest[..] == [0u8; 32][..]
        )),
        "{:?}",
        ops(&commit)
    );
}

#[test]
fn content_l_went_back_to_while_its_edit_was_in_flight_is_sent_after_a_loss() {
    let mut trees = trees(&[synced("n", file(ROOT, "n", 1))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "p", 2);
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 101));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
    let in_flight = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        ..remote(file(ROOT, "n", 2))
    };
    trees.set_remote_edited_here("n".into(), in_flight);
    trees.set_local("n".into(), local(file(ROOT, "n", 1), 102));
    trees.settle();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    trees.set_listed_remote("n".into(), remote_at(file(ROOT, "n", 7), 1));
    trees.remove_remote("p");
    trees.forget_lost(
        trashed,
        edited,
        synced,
        trees.moved_here(),
        untaken,
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        ops(&commit).iter().any(|op| matches!(
            op,
            op::Op::CreateFile(proto::CreateFile { content: Some(content), .. })
                | op::Op::UpdateContent(proto::UpdateContent { content: Some(content), .. })
                if content.manifest_digest[..] == [1u8; 32][..]
        )),
        "{:?}",
        ops(&commit)
    );
}

#[test]
fn an_untaken_edit_another_device_moved_after_the_loss_stays_where_it_moved() {
    let lost_move = RemoteNode {
        meta_rev: 4,
        content_rev: 3,
        ..remote(file("b", "B", 2))
    };
    let held = Some(file("b", "A", 1));
    let mut trees = trees(&[
        synced("b", dir(ROOT, "b")),
        synced("c", dir(ROOT, "c")),
        ("a", held.clone(), held, Some(lost_move)),
    ]);
    let untaken = trees.untaken_changes(|seq| seq == 4);
    assert_eq!(
        untaken,
        [(
            "a".to_owned(),
            Untaken::Edit {
                moved_in_lost_commit: true
            }
        )]
    );
    let moved_after = RemoteNode {
        meta_rev: 6,
        ..remote(file("c", "A", 1))
    };
    trees.set_listed_remote("a".into(), moved_after);
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        untaken,
        Default::default(),
        3,
        0,
    );
    trees.settle();
    assert_eq!(trees.class("a"), Class::Remote);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn content_l_went_back_to_while_its_equal_edit_was_in_flight_is_sent_after_a_loss() {
    let mut trees = trees(&[synced("n", file(ROOT, "n", 1))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "p", 2);
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 101));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
    let resent = RemoteNode {
        content_rev: 3,
        ..remote(file(ROOT, "n", 2))
    };
    trees.set_remote("n".into(), resent);
    let in_flight = RemoteNode {
        content_rev: 3,
        edit_rev: 4,
        ..remote(file(ROOT, "n", 2))
    };
    trees.set_remote_edited_here("n".into(), in_flight);
    trees.set_local("n".into(), local(file(ROOT, "n", 1), 102));
    trees.settle();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    trees.set_listed_remote("n".into(), remote_at(file(ROOT, "n", 7), 1));
    trees.remove_remote("p");
    trees.forget_lost(
        trashed,
        edited,
        synced,
        trees.moved_here(),
        untaken,
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        ops(&commit).iter().any(|op| matches!(
            op,
            op::Op::CreateFile(proto::CreateFile { content: Some(content), .. })
                | op::Op::UpdateContent(proto::UpdateContent { content: Some(content), .. })
                if content.manifest_digest[..] == [1u8; 32][..]
        )),
        "{:?}",
        ops(&commit)
    );
}

#[test]
fn content_this_devices_restore_set_is_not_kept_beside_it_after_a_loss() {
    let mut trees = trees(&[synced("n", file(ROOT, "n", 1))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "p", 2);
    let restored = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        ..remote(file(ROOT, "n", 3))
    };
    trees.set_remote_made_here("n".into(), restored.clone());
    trees.settle();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    trees.set_listed_remote("n".into(), restored);
    trees.remove_remote("p");
    trees.forget_lost(
        trashed,
        edited,
        synced,
        trees.moved_here(),
        untaken,
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    let kept = (sent.iter().flatten()).any(|op| {
        matches!(
            op,
            op::Op::CreateFile(proto::CreateFile { content: Some(content), .. })
                | op::Op::UpdateContent(proto::UpdateContent { content: Some(content), .. })
                if content.manifest_digest[..] == [1u8; 32][..]
        )
    });
    assert!(!kept, "{sent:?}");
}

#[test]
fn a_lost_move_and_edit_are_sent_again_when_another_file_held_the_old_name() {
    for occupied in [false, true] {
        let mut trees = trees(&[
            synced("n", file(ROOT, "x", 1)),
            synced("q", file(ROOT, "z", 2)),
        ]);
        trees.set_remote("n".into(), version(remote(file(ROOT, "x", 1)), "v1"));
        trees.set_remote("q".into(), version(remote(file(ROOT, "z", 2)), "vq"));
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        trees.set_local("n".into(), local(file(ROOT, "y", 3), 1));
        trees.settle();
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        let changed = RemoteNode {
            meta_rev: 2,
            content_rev: 2,
            edit_rev: 2,
            version_id: "v2".into(),
            ..remote(file(ROOT, "y", 3))
        };
        trees.set_remote("n".into(), changed);
        let applied = commit.ops.iter().map(|_| outcome(op_result::Code::Applied));
        commits.received(result(2, applied.collect())).unwrap();
        assert!(commits.acknowledge(&mut trees, 2));
        trees.settle();
        if occupied {
            let moved = version(remote_at(file(ROOT, "x", 2), 3), "vq");
            trees.set_remote("q".into(), moved);
            trees.set_local("q".into(), local(file(ROOT, "x", 2), 2));
            trees.settle();
        }
        assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
        let trashed = trees.trashed_here();
        let edited = trees.edited_here();
        let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
        let moved = trees.moved_here();
        let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
        let records = trees.remote_records();
        trees.set_listed_remote("n".into(), version(remote(file(ROOT, "x", 1)), "v1"));
        trees.set_listed_remote("q".into(), version(remote(file(ROOT, "z", 2)), "vq"));
        trees.forget_lost(
            trashed,
            edited,
            synced,
            moved,
            untaken,
            records,
            commits.held_through(),
            commits.accepted(),
        );
        trees.settle();
        let sent = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
        let edits_n = |op: &op::Op| {
            matches!(op, op::Op::UpdateContent(update) if update.node_id == "n" && update.content == Some(content(3)))
        };
        assert!(
            sent.as_ref().is_some_and(|ops| ops.iter().any(edits_n)),
            "occupied {occupied}: {sent:?}"
        );
        assert!(
            sent.iter().flatten().all(|op| !matches!(op, op::Op::CreateFile(_))),
            "occupied {occupied}: {sent:?}"
        );
    }
}

#[test]
fn a_file_another_device_edited_and_deleted_before_a_loss_stays_deleted() {
    let held = Some(file(ROOT, "a", 1));
    let deleted = RemoteNode {
        content_rev: 4,
        meta_rev: 5,
        version_id: "v3".into(),
        ..trashed(file(ROOT, "a", 2))
    };
    let mut trees = trees(&[("a", held.clone(), held, Some(deleted))]);
    let untaken = trees.untaken_changes(|seq| seq > 2);
    assert_eq!(untaken, [("a".to_owned(), Untaken::Delete)]);
    let records = trees.remote_records();
    trees.set_listed_remote("a".into(), remote(file(ROOT, "a", 0)));
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        untaken,
        records,
        2,
        0,
    );
    trees.settle();
    assert!(trees.unsent_edits().is_empty());
    assert_eq!(trees.class("a"), Class::Remote);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_folder_another_device_deleted_before_a_loss_that_took_it_stays_deleted() {
    let mut trees = trees(&[
        (
            "d",
            Some(dir(ROOT, "d")),
            Some(dir(ROOT, "d")),
            Some(trashed(dir(ROOT, "d"))),
        ),
        (
            "a",
            Some(file("d", "a", 1)),
            Some(file("d", "a", 1)),
            Some(trashed(file("d", "a", 1))),
        ),
    ]);
    let untaken = trees.untaken_changes(|_| false);
    assert_eq!(
        untaken,
        [
            ("a".to_owned(), Untaken::Delete),
            ("d".to_owned(), Untaken::Delete)
        ]
    );
    let records = trees.remote_records();
    trees.remove_remote("a");
    trees.remove_remote("d");
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        untaken,
        records,
        0,
        0,
    );
    trees.settle();
    assert_eq!(
        (trees.class("d"), trees.class("a")),
        (Class::Remote, Class::Remote)
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}
