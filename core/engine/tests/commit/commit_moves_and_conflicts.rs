// SPDX-License-Identifier: Apache-2.0

#[test]
fn local_changes_become_ops_with_parents_first() {
    let mut trees = trees(&[
        ("a", Some(file("z", "f", 1)), None, None),
        ("z", Some(dir(ROOT, "d")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [
            op::Op::CreateDir(proto::CreateDir {
                node_id: "z".into(),
                parent_id: ROOT.into(),
                name: "d".into(),
                pending_for: String::new(),
            }),
            op::Op::CreateFile(proto::CreateFile {
                node_id: "a".into(),
                parent_id: "z".into(),
                name: "f".into(),
                content: Some(content(1)),
            }),
        ]
    );
    assert_eq!(
        (
            commit.history_id.as_str(),
            commit.base_seq,
            commit.device_seq
        ),
        (HISTORY, 7, 1)
    );
    assert_eq!(
        build(&mut commits, &mut trees, &mut ids),
        None,
        "one commit in flight"
    );
}

/// Neither move of a swap is valid before the other, so one node moves to a pending name first and to its own name
/// last, in one commit, and each node keeps its node and its exact name.
#[test]
fn a_name_swap_goes_through_a_pending_name() {
    let mut trees = trees(&swapped());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [
            op::Op::Move(proto::Move {
                from: None,
                node_id: "x".into(),
                new_parent_id: ROOT.into(),
                new_name: "b (pending)".into(),
                pending_for: "b".into(),
            }),
            mv("y", ROOT, "a"),
            mv("x", ROOT, "b")
        ]
    );
}

/// A swap that the rest of the commit leaves no room for waits for the next commit, so its pending name never
/// reaches the server.
#[test]
fn a_name_swap_that_does_not_fit_the_commit_waits_for_the_next_one() {
    let mut rows = swapped();
    rows.push(("n", Some(file(ROOT, "new", 3)), None, None));
    let mut trees = trees(&rows);
    let state = koidrive_engine::state::CommitsState {
        cap: Some(2),
        ..Commits::default().state()
    };
    let (mut commits, mut ids) = (Commits::from_state(state).unwrap(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(ops(&commit).as_slice(), [op::Op::CreateFile(create)] if create.name == "new"),
        "{:?}",
        ops(&commit)
    );
}

/// A move onto a name that a node keeps is no cycle, and is sent for the server to resolve (R1).
#[test]
fn a_move_onto_a_name_another_node_keeps_is_sent_for_the_server_to_resolve() {
    let mut trees = trees(&[
        (
            "x",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        synced("y", file(ROOT, "b", 2)),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("x", ROOT, "b")]);
}

#[test]
fn a_delete_waits_until_a_child_is_moved_out_and_covers_its_subtree() {
    let mut trees = trees(&[
        synced("a", file("d", "keep", 1)),
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        (
            "e",
            None,
            Some(file("d", "gone", 2)),
            Some(remote(file("d", "gone", 2))),
        ),
    ]);
    let mut moved = trees.local().get("a").unwrap().clone();
    moved.shape.parent = ROOT.into();
    trees.set_local("a".into(), moved);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("a", ROOT, "keep"), delete("d")]);
    assert_eq!(
        commits.in_flight().unwrap().deletes["d"],
        ["d".to_string(), "e".to_string()]
    );
}

#[test]
fn a_delete_keeps_what_changed_remotely_and_the_folders_it_needs() {
    let mut trees = trees(&[
        (
            "c",
            None,
            Some(file("d", "c", 1)),
            Some(remote(file("d", "c", 2))),
        ),
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        (
            "e",
            None,
            Some(file("d", "e", 1)),
            Some(remote(file("d", "e", 1))),
        ),
        (
            "n",
            None,
            Some(dir(ROOT, "n")),
            Some(remote(dir(ROOT, "n"))),
        ),
        ("m", None, None, Some(remote(file("n", "new", 3)))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("e")]);
    for id in ["c", "n", "m"] {
        assert_eq!(trees.synced().live(id), None, "{id}");
        assert_eq!(trees.class(id), Class::Remote, "{id}");
    }
    assert_eq!(trees.synced().live("d"), Some(&dir(ROOT, "d")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.synced().live("e"), None);
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
    assert_eq!(trees.synced().live("d"), None);
    assert_eq!(trees.class("d"), Class::Remote);
}

#[test]
fn content_edited_on_both_sides_is_forked_into_a_new_node() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "f", 2)),
        Some(file(ROOT, "f", 1)),
        Some(remote(file(ROOT, "f", 3))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let [op::Op::CreateFile(create)] = <[op::Op; 1]>::try_from(ops(&commit)).unwrap() else {
        panic!("{commit:?}");
    };
    assert_eq!(
        (
            create.parent_id.as_str(),
            create.name.as_str(),
            create.content
        ),
        (ROOT, "f", Some(content(2)))
    );
    assert_eq!(trees.local().get(&create.node_id).unwrap().identity.ino, 1);
    assert_eq!(trees.local().get("f"), None);
    assert_eq!(trees.class("f"), Class::Remote);
}

#[test]
fn an_edit_on_both_sides_whose_write_a_file_beside_it_holds_downloads_r_instead_of_a_fork() {
    let mut trees = trees(&[
        (
            "f",
            Some(file(ROOT, "f", 2)),
            Some(file(ROOT, "f", 1)),
            Some(remote(file(ROOT, "f", 3))),
        ),
        synced("c", file(ROOT, "f (conflict from pc 2026-11-09)", 2)),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(trees.local().get("f").is_some());
    assert_eq!(trees.class("f"), Class::Remote);
}

#[test]
fn an_edit_on_both_sides_is_forked_when_this_device_changed_the_file_beside_it_that_holds_its_write()
 {
    let cases = [("deleted", None), ("overwritten", Some(file(ROOT, "c", 4)))];
    for (case, local) in cases {
        let mut trees = trees(&[
            (
                "f",
                Some(file(ROOT, "f", 2)),
                Some(file(ROOT, "f", 1)),
                Some(remote(file(ROOT, "f", 3))),
            ),
            (
                "c",
                local,
                Some(file(ROOT, "c", 2)),
                Some(remote(file(ROOT, "c", 2))),
            ),
        ]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        let created = (ops(&commit).iter()).any(
            |op| matches!(op, op::Op::CreateFile(create) if create.content == Some(content(2))),
        );
        assert!(created, "{case}: {commit:?}");
    }
}

#[test]
fn an_edit_on_both_sides_is_forked_unless_the_same_write_is_beside_it() {
    let later = |shape: Shape| Shape {
        content: Some(Content {
            mtime_ns: 1,
            ..content(2)
        }),
        ..shape
    };
    let cases = [
        (
            "same bytes written later",
            (ROOT, "f"),
            later(file(ROOT, "g", 2)),
        ),
        (
            "same write in another folder",
            (ROOT, "f"),
            file("d", "g", 2),
        ),
        ("edit moved away from r", ("d", "f"), file(ROOT, "g", 2)),
    ];
    for (case, (parent, name), held) in cases {
        let mut trees = trees(&[
            synced("d", dir(ROOT, "d")),
            (
                "f",
                Some(file(parent, name, 2)),
                Some(file(ROOT, "f", 1)),
                Some(remote(file(ROOT, "f", 3))),
            ),
            synced("c", held),
        ]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        let created = (ops(&commit).iter()).any(
            |op| matches!(op, op::Op::CreateFile(create) if create.content == Some(content(2))),
        );
        assert!(created, "{case}: {commit:?}");
        assert_eq!(trees.local().get("f"), None, "{case}");
    }
}

#[test]
fn an_edit_against_a_remote_move_is_sent_alone() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "a", 2)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "b", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("f", 2)]);
}

#[test]
fn an_edit_and_a_move_against_a_remote_delete_are_sent_move_first() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "b", 2)),
        Some(file(ROOT, "a", 1)),
        Some(trashed(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", ROOT, "b"), update("f", 2)]);
}

#[test]
fn a_move_that_closes_a_cycle_against_r_is_reverted() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir("q", "p")),
            Some(dir(ROOT, "p")),
            Some(remote(dir(ROOT, "p"))),
        ),
        (
            "q",
            Some(dir(ROOT, "q")),
            Some(dir(ROOT, "q")),
            Some(remote(dir("p", "q"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.synced().live("p"), Some(&dir(ROOT, "p")));
    assert!(trees.reverted().contains("p"));
    moves_back(&mut trees, &mut ids, "p");
}

#[test]
fn a_reverted_move_brings_back_the_folders_l_deleted_on_its_way() {
    let mut trees = trees(&[
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        (
            "p",
            Some(dir("q", "p")),
            Some(dir("d", "p")),
            Some(remote(dir("d", "p"))),
        ),
        (
            "q",
            Some(dir(ROOT, "q")),
            Some(dir(ROOT, "q")),
            Some(remote(dir("p", "q"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    let reverted: Vec<&str> = trees.reverted().iter().map(String::as_str).collect();
    assert_eq!(reverted, ["d", "p"]);
    let mut effects = Effects::default();
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        let identity = FileIdentity {
            dev: 1,
            ino: 99,
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
        trees.settle();
    }
    for id in ["d", "p", "q"] {
        assert_eq!(
            trees.local().get(id).map(|n| &n.shape),
            trees.remote().live(id),
            "{id}"
        );
    }
    trees.settle();
    assert!(trees.work().is_empty());
}

#[test]
fn a_folder_a_new_one_may_be_merged_into_is_not_deleted_in_the_same_commit() {
    let mut trees = trees(&[
        (
            "o",
            None,
            Some(dir(ROOT, "a")),
            Some(remote(dir(ROOT, "a"))),
        ),
        (
            "f",
            Some(file("n", "f", 1)),
            Some(file("o", "f", 1)),
            Some(remote(file("o", "f", 1))),
        ),
        ("n", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let create = op::Op::CreateDir(proto::CreateDir {
        node_id: "n".into(),
        parent_id: ROOT.into(),
        name: "a".into(),
        pending_for: String::new(),
    });
    assert_eq!(ops(&commit), [create, mv("f", "n", "f")]);
}

#[test]
fn a_folder_created_where_a_moving_folder_still_is_takes_a_pending_name_first() {
    let mut trees = trees(&[
        (
            "h",
            Some(dir("n", "a")),
            Some(dir(ROOT, "a")),
            Some(remote(dir(ROOT, "a"))),
        ),
        ("n", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    let create = op::Op::CreateDir(proto::CreateDir {
        node_id: "n".into(),
        parent_id: ROOT.into(),
        name: "a (pending)".into(),
        pending_for: "a".into(),
    });
    assert_eq!(ops(&first), [create, mv("h", "n", "a")]);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    trees.set_remote("n".into(), remote(dir(ROOT, "a (pending)")));
    trees.set_remote("h".into(), remote(dir("n", "a")));
    let second = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&second), [owed_mv("n", ROOT, "a", "a (pending)")]);
}

#[test]
fn a_folder_created_where_a_moving_file_still_is_takes_a_pending_name_first() {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "ß", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        ("n", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    let create = op::Op::CreateDir(proto::CreateDir {
        node_id: "n".into(),
        parent_id: ROOT.into(),
        name: "a (pending)".into(),
        pending_for: "a".into(),
    });
    assert_eq!(ops(&first), [create, mv("h", "n", "ß")]);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    trees.set_remote("n".into(), remote(dir(ROOT, "a (pending)")));
    trees.set_remote("h".into(), remote(file("n", "ß", 1)));
    let second = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&second), [owed_mv("n", ROOT, "a", "a (pending)")]);
}

#[test]
fn a_renamed_result_reverts_the_local_name_to_the_server_one() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    assert_eq!(
        trees.synced().live("a"),
        Some(&file(ROOT, "x (conflict)", 1))
    );
    assert!(trees.reverted().contains("a"));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    moves_back(&mut trees, &mut ids, "a");
}

/// Another device created a file at x first, so the server gives this device's new file another
/// name, and the two are a conflict. A build before the local file takes the server's name, as
/// while its move waits for a retry, keeps the question: the name L holds is not the person's move.
#[test]
fn a_conflict_about_a_renamed_create_stays_open_while_the_local_file_waits_for_its_name() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("o".into(), remote(file(ROOT, "x", 2)));
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    assert!(trees.reverted().contains("a"));
    let asked = commits.questions().to_vec();
    assert_eq!(asked.len(), 1);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(commits.questions(), asked);
}

#[test]
fn a_conflict_survives_an_effect_whose_s_update_waits() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "x", 1)),
            Some(file(ROOT, "old", 1)),
            Some(remote(file(ROOT, "old", 1))),
        ),
        (
            "p",
            Some(dir(ROOT, "x (conflict)")),
            Some(dir(ROOT, "x (conflict)")),
            Some(remote(dir(ROOT, "q"))),
        ),
    ]);
    trees.set_local(
        "p".into(),
        LocalNode {
            parked: true,
            ..local(dir(ROOT, "x (conflict)"), 2)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("n", ROOT, "x")]);
    trees.set_remote("o".into(), remote(file(ROOT, "x", 2)));
    trees.set_remote("n".into(), remote(file(ROOT, "x (conflict)", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![OpResult {
            name: "x (conflict)".into(),
            ..outcome(op_result::Code::AppliedRenamed)
        }],
    );
    let asked = commits.questions().to_vec();
    assert_eq!(asked.len(), 1);
    assert!(trees.waiting().contains_key("n"));
    assert_eq!(trees.check(), Ok(()));
    let mut effects = Effects::default();
    let record = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert_eq!(record.node, "n");
    effects
        .done(&mut trees, commits.in_flight(), &record.effect_id, None)
        .unwrap();
    trees.settle();
    for _ in 0..4 {
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        if record.node == "p" {
            effects.failed(&mut trees, &record.effect_id).unwrap();
        } else {
            effects
                .done(
                    &mut trees,
                    commits.in_flight(),
                    &record.effect_id,
                    Some(local(file(ROOT, "x", 2), 10).identity),
                )
                .unwrap();
        }
        trees.settle();
    }
    assert!(
        effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .is_empty()
    );
    assert!(trees.waiting().contains_key("n"));
    assert_eq!(trees.local().live("n"), trees.remote().live("n"));
    assert_eq!(trees.check(), Ok(()));
    build(&mut commits, &mut trees, &mut ids);
    assert_eq!(commits.questions(), asked);
}

#[test]
fn a_parked_fork_recovers_after_its_empty_local_parent_is_deleted() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        (
            "a",
            Some(file("d", "x", 2)),
            Some(file("d", "x (conflict)", 1)),
            Some(remote(file("d", "y", 1))),
        ),
        (
            "c",
            Some(file("d", "y", 4)),
            Some(file("d", "y (conflict)", 4)),
            Some(remote(file("d", "x", 4))),
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
    assert!(matches!(&parking.effect, Effect::Move { to: Location::Parked(id), .. } if id == "a"));
    effects
        .done(&mut trees, commits.in_flight(), &parking.effect_id, None)
        .unwrap();
    trees.set_remote("a".into(), trashed(file("d", "y", 1)));
    trees.set_remote("c".into(), trashed(file("d", "x", 4)));
    trees.remove_local("c");
    trees.remove_local("d");
    trees.settle();
    assert_eq!(trees.check(), Ok(()));
    assert!(
        effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .is_empty()
    );
    for ino in 10..26 {
        trees.settle();
        assert_eq!(trees.check(), Ok(()));
        if let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        {
            effects
                .done(
                    &mut trees,
                    commits.in_flight(),
                    &record.effect_id,
                    Some(local(dir(ROOT, "d"), ino).identity),
                )
                .unwrap();
            continue;
        }
        let Some(sent) = build(&mut commits, &mut trees, &mut ids) else {
            continue;
        };
        for op in ops(&sent) {
            match op {
                op::Op::CreateFile(create) => {
                    trees.set_remote(
                        create.node_id,
                        remote(Shape {
                            parent: create.parent_id,
                            name: create.name,
                            kind: Kind::File,
                            content: create.content,
                        }),
                    );
                }
                op::Op::Delete(deleted) => {
                    assert_eq!(deleted.node_id, "d");
                    trees.set_remote("d".into(), trashed(dir(ROOT, "d")));
                }
                other => panic!("unexpected recovery op: {other:?}"),
            }
        }
        acknowledge(
            &mut commits,
            &mut trees,
            vec![outcome(op_result::Code::Applied); sent.ops.len()],
        );
    }
    trees.settle();
    assert_eq!(trees.check(), Ok(()));
    assert!(
        trees.local().iter().all(|(_, node)| !node.parked),
        "parked edited fork has no effect and no commit: {:?}",
        trees.work()
    );
    assert!(trees.work().is_empty());
    let (id, fork) = (trees.remote().iter())
        .find(|(_, node)| !node.trashed && node.shape.content == Some(content(2)))
        .expect("the edited fork is on the server");
    assert!(trees.remote().live(&fork.shape.parent).is_some());
    assert_eq!(fork.shape.name, "x");
    assert_eq!(
        trees.local().get(id).map(|node| &node.shape),
        Some(&fork.shape)
    );
}

/// Another device renames and rewrites n, and the disk takes both, so L and R agree. S cannot take
/// the new name yet, because the folder p it holds there is parked on its way to q. S still takes
/// the content both sides hold, so the other device's next edit is not taken for a concurrent one
/// and forked: once p has moved, S takes the name and the edit is a remote change.
#[test]
fn a_download_s_cannot_place_yet_is_not_forked_by_the_next_remote_edit() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir(ROOT, "y")),
            Some(dir(ROOT, "y")),
            Some(remote(dir(ROOT, "q"))),
        ),
        (
            "n",
            Some(file(ROOT, "y", 2)),
            Some(file(ROOT, "x", 1)),
            Some(remote(file(ROOT, "y", 2))),
        ),
    ]);
    trees.set_local(
        "p".into(),
        LocalNode {
            parked: true,
            ..local(dir(ROOT, "y"), 1)
        },
    );
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "x", 2)));
    trees.set_remote("n".into(), remote(file(ROOT, "y", 3)));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    trees.set_local("p".into(), local(dir(ROOT, "q"), 1));
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "y", 2)));
    assert_eq!(trees.class("n"), Class::Remote);
}

/// The user renames folder f to a and back to f. A commit built before the second rename was seen
/// moves f to a, and the server renames it to "a (conflict)" because another device holds a, so
/// the node is reverted. The rename back to f is the user's last word: it is sent, and nothing
/// moves the folder to the conflict name.
#[test]
fn a_folder_renamed_back_after_its_move_was_renamed_by_the_server_keeps_the_users_name() {
    for back_before_the_result in [false, true] {
        let mut trees = trees(&[(
            "n",
            Some(dir(ROOT, "a")),
            Some(dir(ROOT, "f")),
            Some(remote(dir(ROOT, "f"))),
        )]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(ops(&commit), [mv("n", ROOT, "a")]);
        if back_before_the_result {
            trees.set_local("n".into(), local(dir(ROOT, "f"), 1));
        }
        trees.set_remote("n".into(), remote(dir(ROOT, "a (conflict)")));
        let renamed = OpResult {
            name: "a (conflict)".into(),
            ..outcome(op_result::Code::AppliedRenamed)
        };
        acknowledge(&mut commits, &mut trees, vec![renamed]);
        trees.set_local("n".into(), local(dir(ROOT, "f"), 1));
        trees.settle();
        assert!(!trees.reverted().contains("n"), "{back_before_the_result}");
        let mut effects = Effects::default();
        let planned = (effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first())
        .map(|record| record.effect.clone());
        assert_eq!(planned, None, "{back_before_the_result}");
        let commit = build(&mut commits, &mut trees, &mut ids);
        assert_eq!(
            commit.map(|commit| ops(&commit)),
            Some(vec![mv("n", ROOT, "f")]),
            "{back_before_the_result}"
        );
    }
}

/// The server lost folder albums, which is parked here, and another device deleted one in it.
/// Once one is removed, albums is not removed too: the commit layer forgets its S first, then the
/// effects move it out of the parked directory, and it is created again.
#[test]
fn a_parked_file_forked_in_a_build_is_created_only_after_the_effects_move_it_back() {
    let mut trees = trees(&[(
        "a",
        Some(file(ROOT, "x", 2)),
        Some(file(ROOT, "x", 1)),
        Some(trashed(file(ROOT, "x", 1))),
    )]);
    trees.set_local(
        "a".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 2), 2)
        },
    );
    trees.revert("a".into());
    trees.settle();
    let (mut commits, mut ids, mut effects) =
        (Commits::default(), IdGen::new(1), Effects::default());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    trees.settle();
    let unparked = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
        .unwrap();
    assert!(matches!(
        &unparked.effect,
        Effect::Move {
            from: Location::Parked(_),
            ..
        }
    ));
    effects
        .done(&mut trees, commits.in_flight(), &unparked.effect_id, None)
        .unwrap();
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(
        &ops(&commit)[..],
        [op::Op::CreateFile(create)] if create.node_id == unparked.node && create.name == "x"
    ));
}

#[test]
fn a_parked_folder_the_server_lost_is_created_again_after_a_delete_empties_it() {
    let mut trees = trees(&[
        (
            "albums",
            Some(dir(ROOT, "albums")),
            Some(dir(ROOT, "albums")),
            None,
        ),
        (
            "one",
            Some(file("albums", "one", 1)),
            Some(file("albums", "one", 1)),
            Some(trashed(file(ROOT, "one", 1))),
        ),
    ]);
    trees.set_local(
        "albums".into(),
        LocalNode {
            parked: true,
            ..local(dir(ROOT, "albums"), 1)
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
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let removed = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
        .unwrap();
    assert_eq!(removed.node, "one");
    assert!(matches!(removed.effect, Effect::RemoveFile { .. }));
    effects
        .done(&mut trees, None, &removed.effect_id, None)
        .unwrap();
    trees.settle();
    assert!(
        effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .is_empty()
    );
    let mut commits = Commits::default();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    trees.settle();
    let unparked = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
        .unwrap();
    assert_eq!(unparked.node, "albums");
    assert!(matches!(unparked.effect, Effect::Move { .. }));
    effects
        .done(&mut trees, commits.in_flight(), &unparked.effect_id, None)
        .unwrap();
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateDir(c)] if c.node_id == "albums"));
}

fn a_trashed_folder_renamed_out_of_its_trashed_parent() -> Vec<Row> {
    vec![
        ("p", None, None, Some(trashed(dir(ROOT, "p")))),
        (
            "z",
            Some(dir(ROOT, "b")),
            Some(dir(ROOT, "z")),
            Some(trashed(dir("p", "z"))),
        ),
    ]
}

fn create_dir(node: &str, parent: &str, name: &str) -> op::Op {
    op::Op::CreateDir(proto::CreateDir {
        pending_for: String::new(),
        node_id: node.into(),
        parent_id: parent.into(),
        name: name.into(),
    })
}

#[test]
fn a_create_into_a_trashed_folder_is_sent_after_the_folders_own_move() {
    let mut rows = a_trashed_folder_renamed_out_of_its_trashed_parent();
    rows.push(("c", Some(dir("z", "c")), None, None));
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [mv("z", ROOT, "b"), create_dir("c", "z", "c")]
    );
}

#[test]
fn a_create_below_a_trashed_folder_is_sent_after_the_folders_own_move() {
    let mut rows = a_trashed_folder_renamed_out_of_its_trashed_parent();
    rows.push((
        "d",
        Some(dir("z", "d")),
        Some(dir("z", "d")),
        Some(trashed(dir("z", "d"))),
    ));
    rows.push(("c", Some(dir("d", "c")), None, None));
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [mv("z", ROOT, "b"), create_dir("c", "d", "c")]
    );
}

#[test]
fn a_move_into_a_trashed_folder_is_sent_after_the_folders_own_move() {
    let mut rows = a_trashed_folder_renamed_out_of_its_trashed_parent();
    rows.push((
        "f",
        Some(file("z", "f", 1)),
        Some(file(ROOT, "f", 1)),
        Some(remote(file(ROOT, "f", 1))),
    ));
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("z", ROOT, "b"), mv("f", "z", "f")]);
}

#[test]
fn an_edit_in_a_trashed_folder_is_sent_after_the_folders_own_move() {
    let mut rows = a_trashed_folder_renamed_out_of_its_trashed_parent();
    rows.push((
        "f",
        Some(file("z", "f", 2)),
        Some(file("z", "f", 1)),
        Some(trashed(file("z", "f", 1))),
    ));
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("z", ROOT, "b"), update("f", 2)]);
}

#[test]
fn a_trashed_folder_moving_onto_the_name_of_a_folder_moving_into_it_goes_through_a_pending_name() {
    let rows = vec![
        ("p", None, None, Some(trashed(dir(ROOT, "p")))),
        (
            "a",
            Some(dir(ROOT, "n")),
            Some(dir(ROOT, "a")),
            Some(trashed(dir("p", "a"))),
        ),
        (
            "n",
            Some(dir("a", "n")),
            Some(dir(ROOT, "n")),
            Some(remote(dir(ROOT, "n"))),
        ),
    ];
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [
            op::Op::Move(proto::Move {
                from: None,
                node_id: "a".into(),
                new_parent_id: ROOT.into(),
                new_name: "n (pending)".into(),
                pending_for: "n".into(),
            }),
            mv("n", "a", "n"),
            mv("a", ROOT, "n"),
        ]
    );
}

#[test]
fn a_move_that_closes_a_cycle_against_trashed_folders_in_r_is_reverted() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir("q", "p")),
            Some(dir(ROOT, "p")),
            Some(trashed(dir(ROOT, "p"))),
        ),
        (
            "q",
            Some(dir(ROOT, "q")),
            Some(dir(ROOT, "q")),
            Some(trashed(dir("p", "q"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(trees.reverted().contains("p"));
}

#[test]
fn a_move_into_a_new_folder_inside_a_folder_r_moved_into_it_and_trashed_is_reverted() {
    let mut trees = trees(&[
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(trashed(dir(ROOT, "d"))),
        ),
        (
            "q",
            Some(dir("n", "q")),
            Some(dir("d", "q")),
            Some(trashed(dir("d", "q"))),
        ),
        (
            "p",
            Some(dir(ROOT, "p")),
            Some(dir(ROOT, "p")),
            Some(trashed(dir("q", "p"))),
        ),
        ("n", Some(dir("p", "n")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(trees.reverted().contains("q"));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateDir(create)] if create.node_id == "n" && create.parent_id == "p"),
        "{commit:?}"
    );
}

#[test]
fn a_create_into_a_trashed_folder_that_moves_out_of_a_live_one_goes_first() {
    let mut trees = trees(&[
        (
            "t",
            Some(dir(ROOT, "T2")),
            Some(dir(ROOT, "T")),
            Some(trashed(dir(ROOT, "T"))),
        ),
        (
            "g",
            None,
            Some(file("t", "g", 1)),
            Some(trashed(file("t", "g", 1))),
        ),
        ("h", Some(file("t", "g", 2)), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(create), moved] if create.node_id == "h" && *moved == mv("t", ROOT, "T2")),
        "{commit:?}"
    );
}

#[test]
fn a_delete_made_after_a_move_was_sent_goes_out_after_the_moves_own_record() {
    let mut trees = trees(&[synced("n", dir(ROOT, "pending"))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("n".into(), local(dir(ROOT, "a"), 1));
    trees.settle();
    let sent = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 1)
        .unwrap();
    assert_eq!(ops(sent), [mv("n", ROOT, "a")]);
    trees.remove_local("n");
    trees.set_remote(
        "n".into(),
        RemoteNode {
            meta_rev: 2,
            meta_edit_rev: 1,
            ..remote(dir(ROOT, "a"))
        },
    );
    trees.settle();
    trees.set_remote_made_here(
        "n".into(),
        RemoteNode {
            meta_rev: 2,
            meta_edit_rev: 3,
            ..remote(dir(ROOT, "a"))
        },
    );
    commits
        .received(result(3, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    let next = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 3)
        .cloned();
    assert_eq!(next.as_ref().map(ops), Some(vec![delete("n")]));
}

#[test]
fn another_devices_resend_of_a_place_and_its_content_sends_nothing_back() {
    let mut trees = trees(&[synced("n", file(ROOT, "a", 1))]);
    trees.set_remote(
        "n".into(),
        RemoteNode {
            content_rev: 2,
            meta_rev: 2,
            edit_rev: 1,
            meta_edit_rev: 1,
            ..remote(file(ROOT, "b", 2))
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 2)
        .cloned();
    assert_eq!(sent.as_ref().map(ops), None);
}
