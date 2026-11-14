// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_parked_edit_whose_name_a_new_folder_holds_is_sent() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "ss", 1)),
        Some(file(ROOT, "ss", 1)),
        Some(trashed(file(ROOT, "ß", 1))),
    )]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "ss", 2), 1)
        },
    );
    trees.set_local("n".into(), local(dir(ROOT, "ß"), 2));
    trees.settle();
    let (mut effects, mut commits, mut ids) =
        (Effects::default(), Commits::default(), IdGen::new(1));
    assert_eq!(
        effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .map(|record| record.effect.clone()),
        None
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(uploads(&ops(&commit), 2), "{:?}", ops(&commit));
}

#[test]
fn a_blocker_at_its_r_position_is_not_parked() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "ss", 1)),
        Some(file(ROOT, "ss", 1)),
        Some(trashed(file(ROOT, "ß", 1))),
    )]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "ss", 2), 1)
        },
    );
    trees.set_local("n".into(), local(dir(ROOT, "ß"), 2));
    trees.set_remote("n".into(), remote(dir(ROOT, "ß")));
    trees.settle();
    let mut state = trees.state();
    state.waiting.insert(
        "n".into(),
        Waiting {
            shape: Some(dir(ROOT, "ß")),
            origin: Origin::Commit(1),
        },
    );
    trees.restore(state).unwrap();
    trees.rules_scanned();
    assert_eq!(trees.check(), Ok(()));
    let (mut effects, mut commits, mut ids) =
        (Effects::default(), Commits::default(), IdGen::new(1));
    assert!(
        effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .is_empty()
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(uploads(&ops(&commit), 2), "{:?}", ops(&commit));
}

#[test]
fn a_parked_edit_forked_for_a_concurrent_edit_keeps_its_parked_entry() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "ss", 1)),
        Some(file(ROOT, "ss", 1)),
        Some(remote(file(ROOT, "ß", 3))),
    )]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "ss", 2), 1)
        },
    );
    trees.set_local("n".into(), local(dir(ROOT, "ß"), 2));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(uploads(&ops(&commit), 2), "{:?}", ops(&commit));
    let (fork, node) = (trees.local().iter())
        .find(|(_, node)| node.identity.ino == 1)
        .unwrap();
    assert_ne!(fork, "h");
    assert!(node.parked);
    assert_eq!(trees.parked_name(fork), "h");
}

#[test]
fn an_edit_made_while_an_unsent_edit_is_out_stays_unsent_when_that_edit_is_acknowledged() {
    let (mut trees, mut commits, mut ids) = an_edit_to_the_content_a_lost_commit_replaced();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
    trees.set_local("n".into(), local(file(ROOT, "n", 5), 1));
    let other = RemoteNode {
        content_rev: 4,
        edit_rev: 4,
        ..remote(file(ROOT, "n", 5))
    };
    trees.set_remote("n".into(), other);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.unsent_edits().iter().collect::<Vec<_>>(), ["n"]);
    let resent = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        ..remote(file(ROOT, "n", 6))
    };
    trees.set_remote("n".into(), resent);
    trees.settle();
    assert_eq!(trees.class("n"), Class::Concurrent);
}

#[test]
fn an_edit_of_a_file_whose_chunk_list_r_leaves_out_counts_for_loss_recovery() {
    let chunk = proto::ChunkRef {
        hash: vec![7; 32],
        size: 3,
        encoding: 0,
    };
    let listed = Shape {
        content: Some(Content {
            chunks: vec![chunk],
            chunk_count: 1,
            ..content(2)
        }),
        ..file(ROOT, "n", 2)
    };
    let unlisted = RemoteNode {
        content_rev: 4,
        edit_rev: 4,
        ..remote(file(ROOT, "n", 2))
    };
    let sent = listed.content.clone();
    let mut trees = trees(&[("n", Some(listed.clone()), Some(listed), Some(unlisted))]);
    trees.record_edited("n", sent.as_ref());
    assert_eq!(trees.edited_here(), [("n".to_owned(), 4)]);
}

#[test]
fn an_edit_of_a_file_in_the_trash_after_a_loss_is_kept_from_a_resend_after_a_restore_of_its_bytes()
{
    let mut trees = trees(&[synced("n", file(ROOT, "n", 3))]);
    let commits = Commits::default();
    let (trashed_here, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), trashed(file(ROOT, "n", 2)));
    trees.forget_lost(
        trashed_here,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert!(trees.unsent_edits().is_empty());
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "n", 2), 3));
    trees.settle();
    let resent = RemoteNode {
        content_rev: 4,
        edit_rev: 4,
        ..remote_at(file(ROOT, "n", 3), 3)
    };
    trees.set_remote("n".into(), resent);
    trees.settle();
    assert_eq!(trees.class("n"), Class::Concurrent);
}

#[test]
fn a_parked_edit_whose_name_goes_to_a_folder_this_device_excludes_is_not_held_back() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "x", 1)),
        Some(file(ROOT, "x", 1)),
        Some(remote(file(ROOT, "X", 1))),
    )]);
    trees.set_rules::<&str>(&[], Some(RuleFile::parse(b"x/\n")));
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let park = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert!(matches!(
        &park.effect,
        Effect::Move {
            to: Location::Parked(_),
            ..
        }
    ));
    effects
        .done(&mut trees, None, &park.effect_id, None)
        .unwrap();
    let mut edited = trees.local().get("h").unwrap().clone();
    edited.shape.content = Some(content(2));
    trees.set_local("h".into(), edited);
    trees.set_remote("h".into(), trashed(file(ROOT, "X", 1)));
    trees.set_remote("n".into(), remote(dir(ROOT, "x")));
    trees.settle();
    assert!(trees.excluded("n") && !trees.excluded("h"));
    let back = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned();
    assert_eq!(
        back.map(|record| record.effect),
        Some(Effect::Move {
            from: Location::Parked("h".into()),
            identity: local(file(ROOT, "x", 1), 1).identity,
            to: Location::At {
                base: koidrive_engine::effects::Base::Root,
                dirs: vec![],
                name: "x".into(),
            },
        })
    );
}

#[test]
fn a_parked_edit_whose_name_a_synced_folder_holds_is_sent() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "ss", 1)),
            Some(file(ROOT, "ss", 1)),
            Some(trashed(file(ROOT, "ß", 1))),
        ),
        (
            "n",
            Some(dir(ROOT, "ß")),
            Some(dir(ROOT, "ß")),
            Some(remote(dir(ROOT, "ß"))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "ss", 2), 1)
        },
    );
    trees.settle();
    let (mut effects, mut commits, mut ids) =
        (Effects::default(), Commits::default(), IdGen::new(1));
    assert_eq!(
        effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .map(|record| record.effect.clone()),
        None
    );
    let commit = build(&mut commits, &mut trees, &mut ids);
    assert!(
        commit
            .as_ref()
            .is_some_and(|commit| uploads(&ops(commit), 2)),
        "{:?}",
        commit.as_ref().map(ops)
    );
}

#[test]
fn a_parked_edit_is_sent_before_the_folder_taking_its_name_downloads() {
    let mut trees = trees(&[
        (
            "h",
            None,
            Some(file(ROOT, "x", 1)),
            Some(trashed(file(ROOT, "x", 1))),
        ),
        ("n", None, None, Some(remote(dir(ROOT, "x")))),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 2), 1)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(ops(&sent).iter().any(|op| matches!(
        op,
        op::Op::CreateFile(create) if create.content == Some(content(2))
    )));
}

#[test]
fn a_parked_edit_goes_back_to_its_name_when_another_folder_gets_that_name() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        (
            "h",
            None,
            Some(file(ROOT, "x", 1)),
            Some(trashed(file(ROOT, "x", 1))),
        ),
        ("n", None, None, Some(remote(dir("d", "x")))),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 2), 2)
        },
    );
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let mut moved_back = false;
    while let Some(record) = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        moved_back |= matches!(
            &record.effect,
            Effect::Move { from: Location::Parked(_), to: Location::At { dirs, name, .. }, .. }
                if dirs.is_empty() && name == "x"
        );
        effects
            .done(
                &mut trees,
                None,
                &record.effect_id,
                Some(local(dir("d", "x"), 9).identity),
            )
            .unwrap();
        trees.settle();
    }
    assert!(moved_back);
}

#[test]
fn a_remote_move_waits_exactly_while_a_commit_in_flight_moves_the_node() {
    use error::Code;
    type Exit = Box<dyn Fn(&mut Commits, &mut Trees)>;
    let ok = || vec![outcome(op_result::Code::Applied)];
    let mut exits: Vec<(String, bool, Exit)> = (0..=i32::from(u8::MAX))
        .filter_map(|code| Code::try_from(code).ok())
        .map(|code| {
            let exit: Exit = Box::new(move |commits: &mut Commits, trees: &mut Trees| {
                commits.failed(trees, &refusal(code, 0)).unwrap();
            });
            (format!("{code:?}"), true, exit)
        })
        .collect();
    let others: [(&str, bool, Exit); 7] = [
        (
            "QuotaExceeded with a retry time",
            true,
            Box::new(|commits, trees| {
                commits
                    .failed(trees, &refusal(Code::QuotaExceeded, 1))
                    .unwrap();
            }),
        ),
        (
            "abandoned",
            true,
            Box::new(|commits, _| assert!(commits.abandon())),
        ),
        (
            "lifted to resend",
            true,
            Box::new(|commits, trees| {
                commits.failed(trees, &refusal(Code::Forbidden, 0)).unwrap();
                commits.lift(Lift::Resend).unwrap();
            }),
        ),
        (
            "lifted to replan",
            true,
            Box::new(|commits, trees| {
                commits.failed(trees, &refusal(Code::Forbidden, 0)).unwrap();
                commits.lift(Lift::Replan).unwrap();
            }),
        ),
        (
            "reconnected before its result",
            true,
            Box::new(|commits, _| {
                assert_eq!(commits.connected(&welcome(0)), Reconnect::Resend);
            }),
        ),
        (
            "reconnected to a server that lost it",
            true,
            Box::new(move |commits, _| {
                commits.received(result(1, ok())).unwrap();
                assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
            }),
        ),
        (
            "acknowledged",
            false,
            Box::new(move |commits, trees| {
                commits.received(result(1, ok())).unwrap();
                assert!(commits.acknowledge(trees, 1));
            }),
        ),
    ];
    exits.extend(others.map(|(name, undone, exit)| (name.to_owned(), undone, exit)));
    let mut released = Vec::new();
    for (name, undone, exit) in exits {
        let mut trees = trees(&[synced("n", file(ROOT, "x", 1))]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        trees.set_local("n".into(), local(file(ROOT, "sent", 1), 1));
        let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(ops(&sent), [mv("n", ROOT, "sent")]);
        if undone {
            trees.set_local("n".into(), local(file(ROOT, "x", 1), 1));
        }
        trees.set_remote("n".into(), remote(file(ROOT, "remote", 1)));
        trees.settle();
        let mut effects = Effects::default();
        let planned = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first();
        assert_eq!(planned, None, "{name}");
        exit(&mut commits, &mut trees);
        trees.settle();
        let planned = (effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first())
        .cloned();
        if commits.in_flight().is_some() {
            assert_eq!(planned, None, "{name}");
            continue;
        }
        let next = planned.unwrap_or_else(|| panic!("{name}: R's move is not planned"));
        let to = next.after.as_ref().map(|after| after.name.as_str());
        assert_eq!(to, Some("remote"), "{name}");
        (effects.done(&mut trees, commits.in_flight(), &next.effect_id, None)).unwrap();
        trees.settle();
        assert_eq!(trees.class("n"), Class::InSync, "{name}");
        released.push(name);
    }
    assert_eq!(
        released,
        [
            "LimitExceeded",
            "ReadOnly",
            "UnsupportedVersion",
            "HeadMoved",
            "AheadOfServer",
            "abandoned",
            "lifted to replan",
            "reconnected to a server that lost it",
            "acknowledged",
        ]
    );
}

#[test]
fn a_rename_of_a_node_whose_update_waits_is_kept_from_an_effect_planned_before_it() {
    let (mut trees, commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    let moved = RemoteNode {
        meta_rev: 3,
        meta_edit_rev: 3,
        ..remote(file(ROOT, "z", 1))
    };
    trees.set_remote("n".into(), moved);
    trees.settle();
    let mut effects = Effects::default();
    let record = loop {
        let record = (effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first())
        .cloned()
        .unwrap();
        if record.node == "n" {
            break record;
        }
        (effects.done(&mut trees, commits.in_flight(), &record.effect_id, None)).unwrap();
        trees.settle();
    };
    assert_eq!(
        record.after.as_ref().map(|after| after.name.as_str()),
        Some("z")
    );
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 2));
    trees.settle();
    assert!(trees.waiting().contains_key("n"));
    assert!(!record.applies(&trees, commits.in_flight()));
    let done = effects.done(&mut trees, commits.in_flight(), &record.effect_id, None);
    assert!(done.is_err(), "{done:?}");
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "c", 1)));
}

#[test]
fn a_rename_the_person_makes_while_the_owed_name_is_sent_is_kept() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    effects_for(&mut trees, &mut ids, "a");
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    trees.set_local("a".into(), local(dir(ROOT, "c"), 1));
    trees.set_remote("a".into(), remote(dir(ROOT, "a")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    let next = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&next), [mv("a", ROOT, "c")]);
}

#[test]
fn a_move_the_person_makes_while_the_owed_name_is_sent_is_kept() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("d", dir(ROOT, "d")),
        synced("a", dir(ROOT, "a (pending)")),
    ]);
    effects_for(&mut trees, &mut ids, "a");
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    trees.set_local("a".into(), local(dir("d", "a"), 2));
    trees.set_remote("a".into(), remote(dir(ROOT, "a")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    let next = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&next), [mv("a", "d", "a")]);
}

#[test]
fn a_rename_back_to_the_pending_name_while_another_rename_is_sent_is_kept() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    trees.set_local("a".into(), local(dir(ROOT, "x"), 1));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("a", ROOT, "x")]);
    trees.set_local("a".into(), local(dir(ROOT, "a (pending)"), 1));
    trees.set_remote("a".into(), remote(dir(ROOT, "x")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    let next = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&next), [mv("a", ROOT, "a (pending)")]);
}

#[test]
fn an_excluded_node_at_a_pending_name_gets_no_rename() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    trees.set_rules::<&str>(&[], Some(RuleFile::parse(b"a (pending)/\n")));
    trees.settle();
    assert!(trees.excluded("a"));
    assert!(!commits.due(&trees, "me"));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_held_node_at_a_pending_name_gets_no_rename() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("d", dir(ROOT, "d")),
        synced("a", dir("d", "a (pending)")),
    ]);
    trees.set_rules_unknown("d");
    trees.settle();
    assert!(trees.held("a"));
    assert!(!commits.due(&trees, "me"));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn an_owed_rename_that_lands_is_not_sent_again_once_another_device_names_the_node_back() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    effects_for(&mut trees, &mut ids, "a");
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a"), 1));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (pending)"), 2));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.as_ref()
            .is_none_or(|commit| !ops(commit).contains(&mv("a", ROOT, "a"))),
        "{:?}",
        next.as_ref().map(ops)
    );
}

#[test]
fn a_rename_this_device_makes_off_a_pending_name_ends_the_owed_rename() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    trees.set_local("a".into(), local(dir(ROOT, "c"), 1));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("a", ROOT, "c")]);
    trees.set_remote("a".into(), remote_at(dir(ROOT, "c"), 1));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (pending)"), 2));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.set_local("a".into(), local(dir(ROOT, "a (pending)"), 1));
    trees.settle();
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.as_ref()
            .is_none_or(|commit| !ops(commit).contains(&mv("a", ROOT, "a"))),
        "{:?}",
        next.as_ref().map(ops)
    );
}

#[test]
fn a_rename_another_device_makes_off_a_pending_name_ends_the_owed_rename_before_any_build() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    trees.set_remote("a".into(), remote_at(dir(ROOT, "c"), 1));
    assert!(!commits.acknowledge(&mut trees, 1));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (pending)"), 2));
    assert!(!commits.acknowledge(&mut trees, 2));
    trees.settle();
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.as_ref()
            .is_none_or(|commit| !ops(commit).contains(&mv("a", ROOT, "a"))),
        "{:?}",
        next.as_ref().map(ops)
    );
}

#[test]
fn a_rename_another_device_makes_while_this_device_moves_the_node_ends_the_owed_rename() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("d", dir(ROOT, "d")),
        synced("a", dir(ROOT, "a (pending)")),
    ]);
    trees.set_local("a".into(), local(dir("d", "a (pending)"), 2));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("a", "d", "a (pending)")]);
    trees.set_remote("a".into(), remote_at(dir("d", "a (pending)"), 8));
    assert!(!commits.acknowledge(&mut trees, 8));
    trees.set_remote("a".into(), remote_at(dir("d", "c"), 9));
    assert!(!commits.acknowledge(&mut trees, 9));
    trees.set_remote("a".into(), remote_at(dir("d", "a (pending)"), 10));
    assert!(!commits.acknowledge(&mut trees, 10));
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 10));
    trees.settle();
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.as_ref()
            .is_none_or(|commit| !ops(commit).contains(&mv("a", "d", "a"))),
        "{:?}",
        next.as_ref().map(ops)
    );
}

#[test]
fn a_rename_another_device_makes_once_a_pending_create_lands_ends_the_owed_rename_before_the_result()
 {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "child", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        ("n", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(first.ops.len(), 2);
    trees.remove_local("h");
    trees.remove_local("n");
    trees.set_remote("n".into(), remote_at(dir(ROOT, "a (pending)"), 8));
    trees.set_remote("h".into(), remote_at(file("n", "child", 1), 8));
    assert!(!commits.acknowledge(&mut trees, 8));
    trees.set_remote("n".into(), remote_at(dir(ROOT, "c"), 9));
    trees.set_remote("h".into(), remote_at(file("n", "child", 2), 9));
    assert!(!commits.acknowledge(&mut trees, 9));
    trees.set_remote("n".into(), remote_at(dir(ROOT, "a (pending)"), 10));
    assert!(!commits.acknowledge(&mut trees, 10));
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied); 2]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 10));
    trees.settle();
    let _ = build(&mut commits, &mut trees, &mut ids);
    effects_for(&mut trees, &mut ids, "n");
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.as_ref()
            .is_none_or(|commit| !ops(commit).contains(&mv("n", ROOT, "a"))),
        "{:?}",
        next.as_ref().map(ops)
    );
}

#[test]
fn a_marked_node_gets_its_name_only_once_it_is_restored_from_the_trash() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        ("a", None, None, Some(trashed(dir(ROOT, "a (pending)")))),
        ("z", Some(file(ROOT, "z", 1)), None, None),
    ]);
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(uploads(&ops(&sent), 1), "{:?}", ops(&sent));
    assert!(!ops(&sent).contains(&mv("a", ROOT, "a")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );

    let mut node = trees.remote().get("a").unwrap().clone();
    node.trashed = false;
    node.trash_group = String::new();
    trees.set_remote("a".into(), node);
    trees.set_remote("z".into(), remote(file(ROOT, "z", 1)));
    trees.set_local("a".into(), local(dir(ROOT, "a (pending)"), 3));
    trees.set_synced("a".into(), Some(dir(ROOT, "a (pending)")));
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    let next = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&next), [owed_mv("a", ROOT, "a", "a (pending)")]);
}

#[test]
fn a_free_name_is_taken_on_disk_before_the_rename_is_sent() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    let planned = effects_for(&mut trees, &mut ids, "a");
    assert!(
        matches!(planned.as_slice(), [Effect::Move { .. }]),
        "{planned:?}"
    );
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
    assert_eq!(trees.synced().live("a"), Some(&dir(ROOT, "a (pending)")));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    assert!(!sent.require_head);
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a"), 9));
    commits
        .received(result(9, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 9));
    trees.settle();
    assert_eq!(trees.synced().live("a"), Some(&dir(ROOT, "a")));
    assert_eq!(trees.synced_mark("a"), None);
}

#[test]
fn a_size_refusal_releases_the_finishes_its_commit_carried() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("a", dir(ROOT, "a (pending)")),
        synced("b", dir(ROOT, "a")),
        synced("c", dir(ROOT, "c (pending)")),
        synced("d", dir(ROOT, "c")),
    ]);
    let mut other = trees.remote().get("c").unwrap().clone();
    other.pending_for = Some("c".into());
    other.pending_by = Some("me".into());
    trees.set_remote("c".into(), other);
    trees.settle();
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&first), [owed_mv("a", ROOT, "a", "a (pending)"), owed_mv("c", ROOT, "c", "c (pending)")]);
    assert!(!first.require_head);
    commits
        .failed(&mut trees, &refusal(error::Code::LimitExceeded, 0))
        .unwrap();
    assert!(commits.problems().is_empty());
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.is_some(),
        "an unapplied size refusal must not stall the only pending finish until another commit lands"
    );
}

#[test]
fn only_head_moved_holds_a_finish_until_a_newer_head() {
    use error::Code;
    for code in [
        Code::HeadMoved,
        Code::UnsupportedVersion,
        Code::AheadOfServer,
    ] {
        let (mut commits, mut trees, mut ids) = owing_its_name(&[
            synced("a", dir(ROOT, "a (pending)")),
            synced("b", dir(ROOT, "a")),
        ]);
        let first = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(ops(&first), [owed_mv("a", ROOT, "a", "a (pending)")]);
        commits.failed(&mut trees, &refusal(code, 0)).unwrap();
        let again = build(&mut commits, &mut trees, &mut ids);
        assert_eq!(again.is_none(), code == Code::HeadMoved, "{code:?}");
        if again.is_none() {
            let newer = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 8);
            assert_eq!(ops(newer.unwrap()), [owed_mv("a", ROOT, "a", "a (pending)")]);
        }
    }
}

#[test]
fn a_rewind_releases_a_finish_held_at_the_old_history_head() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("a", dir(ROOT, "a (pending)")),
        synced("b", dir(ROOT, "a")),
    ]);
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(first.base_seq, 7);
    let mut after_rewind = trees.remote().get("a").unwrap().clone();
    after_rewind.meta_rev = 2;
    trees.set_remote("a".into(), after_rewind);
    commits.recover(
        &mut trees,
        0,
        &RecoveryCause::of(RecoveryTrigger::SpaceRewound),
        &mut ids,
        NOW,
    );
    trees.settle();
    assert!(commits.questions().is_empty());
    let next = commits
        .build(&mut trees, &mut ids, NOW, "new-history", "me", 2)
        .cloned();
    assert!(
        next.is_some(),
        "a finish cannot wait for seq 7 from a history that was rewound to seq 2"
    );
}

#[test]
fn a_rename_taken_on_disk_follows_the_server_when_another_user_renames_the_node_first() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "c"), 9));
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "a");
    trees.settle();
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "c")));
    assert_eq!(trees.synced().live("a"), Some(&dir(ROOT, "c")));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_rename_taken_on_disk_is_not_sent_again_once_the_setter_finished_it() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    effects_for(&mut trees, &mut ids, "a");
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a"), 9));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "c"), 10));
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "a");
    trees.settle();
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "c")));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_rename_taken_on_disk_follows_a_later_rename_when_s_could_not_take_the_setters_finish() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("a", dir(ROOT, "a (pending)")),
        synced("f", file(ROOT, "x", 1)),
    ]);
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
    trees.set_local("f".into(), local(file(ROOT, "A", 1), 2));
    trees.set_remote("f".into(), remote_at(file(ROOT, "A", 1), 8));
    trees.settle();
    trees.set_local("f".into(), local(file(ROOT, "b", 1), 2));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a"), 9));
    trees.settle();
    assert_eq!(trees.synced().live("f"), Some(&file(ROOT, "A", 1)));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "c"), 10));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids);
    assert!(
        !sent.iter().flat_map(ops).any(|op| matches!(&op, op::Op::Move(moved) if moved.node_id == "a")),
        "{sent:?}"
    );
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "c")));
}

#[test]
fn a_finish_another_device_makes_keeps_a_remote_write_this_device_never_took() {
    let (_commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", file(ROOT, "a (pending)", 1))]);
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&file(ROOT, "a", 1)));
    trees.set_local("a".into(), local(file(ROOT, "a", 2), 1));
    trees.set_remote(
        "a".into(),
        RemoteNode {
            content_rev: 2,
            edit_rev: 2,
            pending_for: Some("a".into()),
            pending_by: Some("me".into()),
            ..remote(file(ROOT, "a (pending)", 3))
        },
    );
    trees.settle();
    assert_eq!(trees.class("a"), Class::Concurrent);
    trees.set_remote(
        "a".into(),
        RemoteNode {
            content_rev: 2,
            edit_rev: 2,
            meta_rev: 3,
            meta_edit_rev: 1,
            ..remote(file(ROOT, "a", 3))
        },
    );
    trees.settle();
    assert_eq!(
        trees.synced().live("a").and_then(|shape| shape.content.clone()),
        Some(content(1))
    );
    assert_eq!(trees.class("a"), Class::Concurrent);
}

#[test]
fn a_finish_on_a_server_that_keeps_no_marks_requires_the_head() {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "child", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        ("n", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let created = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(ops(&created).first(), Some(op::Op::CreateDir(create)) if create.pending_for == "a")
    );
    for (id, shape) in [("n", dir(ROOT, "a (pending)")), ("h", file("n", "child", 1))] {
        trees.set_remote(
            id.into(),
            RemoteNode {
                meta_edit_rev: 0,
                ..remote_at(shape, 8)
            },
        );
    }
    commits
        .received(result(
            8,
            vec![outcome(op_result::Code::Applied); created.ops.len()],
        ))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    effects_for(&mut trees, &mut ids, "n");
    let finish = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 8)
        .cloned()
        .unwrap();
    assert_eq!(ops(&finish), [owed_mv("n", ROOT, "a", "a (pending)")]);
    assert!(finish.require_head);
}

#[test]
fn a_rename_the_setter_finishes_beside_a_node_holding_its_name_keeps_the_mark_in_s_until_the_disk_follows()
 {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("a", dir(ROOT, "a (pending)")),
        synced("b", dir(ROOT, "a")),
    ]);
    trees.set_synced_mark("a".into(), Some("a".into()));
    let finish = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&finish), [owed_mv("a", ROOT, "a", "a (pending)")]);
    assert!(!finish.require_head);
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a 2"), 9));
    let renamed = OpResult {
        name: "a 2".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    commits.received(result(9, vec![renamed])).unwrap();
    assert!(commits.acknowledge(&mut trees, 9));
    trees.settle();
    assert_eq!(trees.synced().live("a"), Some(&dir(ROOT, "a (pending)")));
    assert_eq!(trees.synced_mark("a"), Some("a"));
    effects_for(&mut trees, &mut ids, "a");
    trees.settle();
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a 2")));
    assert_eq!(trees.synced().live("a"), Some(&dir(ROOT, "a 2")));
    assert_eq!(trees.synced_mark("a"), None);
}

#[test]
fn a_rename_owed_from_before_marks_taken_on_disk_is_sent_requiring_the_head() {
    let mut trees = trees(&[synced("a", dir(ROOT, "a (pending)"))]);
    owing_a_rename_from_before_marks(&mut trees);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    assert!(sent.require_head);
}

#[test]
fn the_setters_owed_rename_its_disk_already_holds_is_a_finish_that_a_delete_retires() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[(
        "a",
        Some(dir(ROOT, "a")),
        Some(dir(ROOT, "a (pending)")),
        Some(remote(dir(ROOT, "a (pending)"))),
    )]);
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    assert!(!sent.require_head);
    commits
        .failed(&mut trees, &refusal(error::Code::HeadMoved, 0))
        .unwrap();
    let mut deleted = trees.remote().get("a").unwrap().clone();
    deleted.trashed = true;
    deleted.meta_rev = 9;
    trees.set_remote("a".into(), deleted);
    trees.settle();
    let after = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 8);
    assert_eq!(after, None);
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), None);
}

#[test]
fn a_folder_deleted_before_its_setter_builds_the_owed_rename_is_not_revived() {
    let mut trees = trees(&[
        ("a", Some(dir(ROOT, "A")), None, None),
        (
            "b",
            Some(file("a", "a", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let created = build(&mut commits, &mut trees, &mut ids).unwrap();
    let [op::Op::CreateDir(create), ..] = &ops(&created)[..] else {
        panic!("{:?}", ops(&created));
    };
    let applied = vec![outcome(op_result::Code::Applied); created.ops.len()];
    assert_eq!(create.pending_for, "A");
    acknowledge(&mut commits, &mut trees, applied);
    let mut deleted = remote_at(dir(ROOT, &create.name), 9);
    deleted.trashed = true;
    deleted.pending_for = Some("A".into());
    deleted.pending_by = Some("me".into());
    trees.set_remote("a".into(), deleted);
    trees.settle();
    assert_eq!(
        commits
            .build(&mut trees, &mut ids, NOW, HISTORY, "me", 9)
            .cloned(),
        None
    );
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), None);
}

#[test]
fn a_rename_owed_from_before_marks_taken_on_disk_follows_a_newer_move_by_another_user() {
    let mut trees = trees(&[synced("a", dir(ROOT, "a (pending)"))]);
    owing_a_rename_from_before_marks(&mut trees);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    effects_for(&mut trees, &mut ids, "a");
    trees.set_remote(
        "a".into(),
        remote_at(dir(ROOT, "chosen by another user"), 9),
    );
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "a");
    trees.settle();
    assert_eq!(
        trees.local().live("a"),
        Some(&dir(ROOT, "chosen by another user"))
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_rename_taken_on_disk_follows_a_newer_mark_that_moves_the_node_to_another_folder() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("d", dir(ROOT, "d")),
        synced("a", dir(ROOT, "a (pending)")),
    ]);
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
    let mut moved = remote_at(dir("d", "a (pending)"), 9);
    moved.pending_for = Some("a".into());
    moved.pending_by = Some("other".into());
    trees.set_remote("a".into(), moved);
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "a");
    trees.settle();
    assert_eq!(
        (trees.local().live("a")).map(|local| local.parent.as_str()),
        Some("d")
    );
}

#[test]
fn a_rename_taken_on_disk_is_not_sent_once_another_user_deletes_the_node() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    effects_for(&mut trees, &mut ids, "a");
    trees.set_remote("a".into(), trashed(dir(ROOT, "a (pending)")));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids);
    assert!(
        sent.as_ref().is_none_or(|sent| ops(sent).is_empty()),
        "{sent:?}"
    );
    effects_for(&mut trees, &mut ids, "a");
    trees.settle();
    assert_eq!(trees.local().live("a"), None);
}

#[test]
fn a_create_a_store_from_before_marks_owed_a_rename_for_is_finished_after_its_result() {
    let mut trees = trees(&[("a", Some(dir(ROOT, "a (pending)")), None, None)]);
    let mut ids = IdGen::new(1);
    let mut commits = Commits::default();
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(
        ops(&sent).as_slice(),
        [op::Op::CreateDir(create)] if create.name == "a (pending)" && create.pending_for.is_empty()
    ));
    owing_a_rename_from_before_marks(&mut trees);
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (pending)"), 8));
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (pending)"), 8));
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
    let finish = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&finish), [owed_mv("a", ROOT, "a", "a (pending)")]);
}

#[test]
fn a_rename_owed_from_before_marks_ends_when_another_user_renames_the_node_before_the_result() {
    let mut trees = trees(&[("a", Some(dir(ROOT, "a (pending)")), None, None)]);
    let mut ids = IdGen::new(1);
    let mut commits = Commits::default();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    owing_a_rename_from_before_marks(&mut trees);
    trees.set_remote(
        "a".into(),
        remote_at(dir(ROOT, "chosen by another user"), 9),
    );
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 9));
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    trees.settle();
    assert_eq!(
        trees.local().live("a"),
        Some(&dir(ROOT, "chosen by another user"))
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_marked_node_takes_its_name_once_the_node_holding_it_moves_away() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("a", dir(ROOT, "a (pending)")),
        (
            "b",
            Some(dir(ROOT, "A (pending)")),
            Some(dir(ROOT, "a")),
            Some(remote(dir(ROOT, "a"))),
        ),
    ]);
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("b", ROOT, "A (pending)")]);
    trees.set_remote("b".into(), remote_at(dir(ROOT, "A (pending) 2"), 8));
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(
        trees.local().live("a").map(|shape| shape.name.as_str()),
        Some("a")
    );
    let finish = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&finish), [owed_mv("a", ROOT, "a", "a (pending)")]);
}

#[test]
fn an_older_server_that_keeps_no_marks_still_gets_the_owed_rename() {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "child", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        ("n", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let created = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(ops(&created).as_slice(), [op::Op::CreateDir(create), ..]
        if create.node_id == "n" && create.pending_for == "a")
    );
    trees.set_remote("n".into(), remote_at(dir(ROOT, "a (pending)"), 8));
    trees.set_remote("h".into(), remote_at(file("n", "child", 1), 8));
    commits
        .received(result(
            8,
            vec![outcome(op_result::Code::Applied); created.ops.len()],
        ))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    let sent = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 8)
        .cloned();
    assert!(
        sent.as_ref()
            .is_some_and(|sent| ops(sent) == [owed_mv("n", ROOT, "a", "a (pending)")]),
        "an older server's omitted mark must not cancel the rename requested on this client: {sent:?}"
    );
}

#[test]
fn a_rename_away_and_back_seen_in_one_batch_ends_the_finish() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "chosen by user"), 9));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (pending)"), 10));
    trees.settle();
    let sent = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 10)
        .cloned();
    assert_eq!(
        sent, None,
        "the automatic rename must not overwrite the user's newer unmarked name"
    );
    effects_for(&mut trees, &mut ids, "a");
    trees.settle();
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a (pending)")));
}

#[test]
fn a_rename_owed_from_before_marks_taken_on_disk_sends_the_users_move_back_after_it() {
    let mut trees = trees(&[synced("a", dir(ROOT, "a (pending)"))]);
    owing_a_rename_from_before_marks(&mut trees);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    trees.set_local("a".into(), local(dir(ROOT, "a (pending)"), 1));
    trees.settle();
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a"), 8));
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    let next = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 8)
        .cloned();
    assert_eq!(
        next.as_ref().map(ops),
        Some(vec![mv("a", ROOT, "a (pending)")])
    );
}

#[test]
fn a_finish_the_disk_took_first_sends_the_users_move_back_after_a_result_with_no_user_change() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    trees.set_local("a".into(), local(dir(ROOT, "a (pending)"), 1));
    trees.settle();
    let finished = RemoteNode {
        shape: dir(ROOT, "a"),
        meta_rev: 8,
        pending_for: None,
        pending_by: None,
        ..trees.remote().get("a").unwrap().clone()
    };
    trees.set_remote("a".into(), finished);
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    let next = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 8)
        .cloned();
    assert_eq!(
        next.as_ref().map(ops),
        Some(vec![mv("a", ROOT, "a (pending)")])
    );
}

#[test]
fn a_finish_sent_before_the_disk_took_it_is_placed_on_disk_after_a_result_with_no_user_change() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("a", dir(ROOT, "a (pending)")),
        synced("b", dir(ROOT, "a")),
    ]);
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    trees.set_remote("b".into(), remote_at(dir(ROOT, "b"), 8));
    let finished = RemoteNode {
        shape: dir(ROOT, "a"),
        meta_rev: 9,
        pending_for: None,
        pending_by: None,
        ..trees.remote().get("a").unwrap().clone()
    };
    trees.set_remote("a".into(), finished);
    commits
        .received(result(9, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 9));
    trees.settle();
    let next = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 9)
        .cloned();
    assert!(
        !next.iter().flat_map(ops).any(|op| matches!(&op, op::Op::Move(moved) if moved.node_id == "a")),
        "{next:?}"
    );
    effects_for(&mut trees, &mut ids, "b");
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
}

#[test]
fn a_finish_refused_then_taken_on_disk_sends_the_users_move_back_after_it() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("a", dir(ROOT, "a (pending)")),
        synced("b", dir(ROOT, "a")),
    ]);
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&first), [owed_mv("a", ROOT, "a", "a (pending)")]);
    commits
        .failed(&mut trees, &refusal(error::Code::HeadMoved, 0))
        .unwrap();
    trees.set_remote("b".into(), remote_at(dir(ROOT, "b"), 8));
    assert!(!commits.acknowledge(&mut trees, 8));
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
    let sent = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 8)
        .cloned()
        .unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    trees.set_local("a".into(), local(dir(ROOT, "a (pending)"), 1));
    trees.settle();
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a"), 9));
    commits
        .received(result(9, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 9));
    trees.settle();
    let next = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 9)
        .cloned();
    assert_eq!(
        next.as_ref().map(ops),
        Some(vec![mv("a", ROOT, "a (pending)")])
    );
}

#[test]
fn a_rename_owed_from_before_marks_made_on_disk_by_the_user_sends_their_move_back_after_it() {
    let mut trees = trees(&[synced("a", dir(ROOT, "a (pending)"))]);
    owing_a_rename_from_before_marks(&mut trees);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("a".into(), local(dir(ROOT, "a"), 1));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    trees.set_local("a".into(), local(dir(ROOT, "a (pending)"), 1));
    trees.settle();
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a"), 8));
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    let next = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 8)
        .cloned();
    assert_eq!(
        next.as_ref().map(ops),
        Some(vec![mv("a", ROOT, "a (pending)")])
    );
}

#[test]
fn a_folder_another_device_moves_at_its_pending_name_keeps_that_name() {
    reparent_a_folder_at_its_pending_name(true);
}

#[test]
fn a_folder_another_device_moves_at_its_pending_name_before_the_answer_keeps_that_name() {
    reparent_a_folder_at_its_pending_name(false);
}

#[test]
fn a_folder_another_device_moves_while_its_owed_name_is_still_taken_keeps_that_place() {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "a", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        ("n", Some(dir(ROOT, "A")), None, None),
        (
            "x",
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "x")),
            Some(remote(dir(ROOT, "x"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    let results = ops(&first)
        .iter()
        .map(|op| match op {
            op::Op::Move(m) if m.node_id == "h" => outcome(op_result::Code::Superseded),
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    trees.set_remote("n".into(), remote_at(dir(ROOT, "A (pending)"), 8));
    commits.received(result(8, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    trees.set_remote("n".into(), remote_at(dir("x", "A (pending)"), 9));
    commits.acknowledge(&mut trees, 9);
    trees.settle();
    let n_moves = |commit: &proto::Commit| {
        ops(commit)
            .into_iter()
            .filter(|op| matches!(op, op::Op::Move(m) if m.node_id == "n"))
            .collect::<Vec<_>>()
    };
    if let Some(commit) = build(&mut commits, &mut trees, &mut ids) {
        assert_eq!(n_moves(&commit), [], "{:?}", ops(&commit));
    }
    assert!(commits.in_flight().is_none());
    trees.set_remote("h".into(), remote_at(file("n", "a", 1), 10));
    commits.acknowledge(&mut trees, 10);
    trees.settle();
    let _ = build(&mut commits, &mut trees, &mut ids);
    effects_for(&mut trees, &mut ids, "n");
    if let Some(commit) = build(&mut commits, &mut trees, &mut ids) {
        assert_eq!(n_moves(&commit), [], "{:?}", ops(&commit));
    }
    let n = &trees.local().get("n").unwrap().shape;
    assert_eq!((n.parent.as_str(), n.name.as_str()), ("x", "A (pending)"));
}

#[test]
fn a_folder_another_device_moves_takes_that_place_once_the_node_on_its_owed_name_stays() {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "a", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        ("n", Some(dir(ROOT, "A")), None, None),
        (
            "x",
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "x")),
            Some(remote(dir(ROOT, "x"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    let results = ops(&first)
        .iter()
        .map(|op| match op {
            op::Op::Move(m) if m.node_id == "h" => outcome(op_result::Code::Superseded),
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    trees.set_remote("n".into(), remote_at(dir(ROOT, "A (pending)"), 8));
    commits.received(result(8, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    trees.set_remote("n".into(), remote_at(dir("x", "A (pending)"), 9));
    commits.acknowledge(&mut trees, 9);
    trees.set_local("h".into(), local(file(ROOT, "a", 1), 1));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids);
    assert_eq!(sent.as_ref().map(ops), None);
    effects_for(&mut trees, &mut ids, "n");
    trees.settle();
    let n = &trees.local().get("n").unwrap().shape;
    assert_eq!((n.parent.as_str(), n.name.as_str()), ("x", "A (pending)"));
    assert_eq!(trees.class("n"), Class::InSync);
}

#[test]
fn a_folder_another_device_deletes_is_deleted_once_the_node_on_its_owed_name_stays() {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "a", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        ("n", Some(dir(ROOT, "A")), None, None),
        (
            "x",
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "x")),
            Some(remote(dir(ROOT, "x"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    let results = ops(&first)
        .iter()
        .map(|op| match op {
            op::Op::Move(m) if m.node_id == "h" => outcome(op_result::Code::Superseded),
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    trees.set_remote("n".into(), remote_at(dir(ROOT, "A (pending)"), 8));
    commits.received(result(8, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    let deleted = RemoteNode {
        trashed: true,
        trash_group: "9:0".into(),
        ..remote_at(dir(ROOT, "A (pending)"), 9)
    };
    trees.set_remote("n".into(), deleted);
    commits.acknowledge(&mut trees, 9);
    trees.set_local("h".into(), local(file(ROOT, "a", 1), 1));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids);
    assert_eq!(sent.as_ref().map(ops), None);
    effects_for(&mut trees, &mut ids, "n");
    trees.settle();
    assert!(trees.local().get("n").is_none());
    assert_eq!(trees.class("n"), Class::InSync);
}

#[test]
fn a_folder_another_device_deletes_keeps_its_owed_name_while_a_parked_file_in_it_holds_an_edit() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        (
            "a",
            Some(dir(ROOT, "a")),
            Some(dir(ROOT, "a (pending)")),
            Some(trashed(dir(ROOT, "a (pending)"))),
        ),
        (
            "f",
            Some(file("a", "c.txt", 2)),
            Some(file("a", "c.txt", 1)),
            Some(trashed(file("a", "c.txt", 1))),
        ),
    ]);
    trees.set_local(
        "f".into(),
        LocalNode {
            parked: true,
            ..local(file("a", "c.txt", 2), 7)
        },
    );
    trees.settle();
    assert!(!commits.acknowledge(&mut trees, 1));
    trees.settle();
    assert_eq!(
        (trees.remote().get("a")).and_then(|remote| remote.pending_for.as_deref()),
        Some("a")
    );
    assert_eq!(trees.class("a"), Class::Concurrent);
    let _ = build(&mut commits, &mut trees, &mut ids);
}

#[test]
fn a_folder_the_user_moves_elsewhere_under_its_owed_name_is_still_moved() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("u", dir(ROOT, "u")),
        synced("x", dir(ROOT, "x")),
        (
            "a",
            Some(dir("u", "a")),
            Some(dir(ROOT, "a (pending)")),
            Some(remote(dir("x", "a (pending)"))),
        ),
    ]);
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("a", "u", "a")]);
}

#[test]
fn a_file_another_device_deletes_while_this_device_edits_it_takes_its_owed_name_once_the_edit_brings_it_back()
 {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[(
        "a",
        Some(file(ROOT, "a", 2)),
        Some(file(ROOT, "a (pending)", 1)),
        Some(trashed(file(ROOT, "a (pending)", 1))),
    )]);
    let mut state = trees.state();
    state.pending_finishes.insert("a".into(), PendingFinish {
        parent: ROOT.into(),
        pending: "a (pending)".into(),
        name: "a".into(),
        origin: FinishOrigin::Mark { meta_rev: 1 },
        disk_first: true,
        base: None,
        sending: false,
    });
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle();
    assert!(!commits.acknowledge(&mut trees, 1));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(!ops(&sent).contains(&owed_mv("a", ROOT, "a", "a (pending)")));
    let revived = RemoteNode {
        pending_for: Some("a".into()),
        pending_by: Some("me".into()),
        ..remote_at(file(ROOT, "a (pending)", 2), 2)
    };
    trees.set_remote("a".into(), revived);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    let next = build(&mut commits, &mut trees, &mut ids);
    assert_eq!(next.as_ref().map(ops), Some(vec![owed_mv("a", ROOT, "a", "a (pending)")]));
}

#[test]
fn a_finish_the_disk_holds_in_another_unicode_form_is_sent_at_its_name() {
    let (name, decomposed, pending) = ("\u{e9}", "e\u{301}", "\u{e9} (pending)");
    let mut trees = trees(&[(
        "a",
        Some(dir(ROOT, decomposed)),
        Some(dir(ROOT, pending)),
        Some(remote(dir(ROOT, pending))),
    )]);
    let marked = RemoteNode {
        pending_for: Some(name.into()),
        pending_by: Some("me".into()),
        ..remote(dir(ROOT, pending))
    };
    trees.set_remote("a".into(), marked);
    let mut state = trees.state();
    state.pending_finishes.insert("a".into(), PendingFinish {
        parent: ROOT.into(),
        pending: pending.into(),
        name: name.into(),
        origin: FinishOrigin::Mark { meta_rev: 1 },
        disk_first: true,
        base: None,
        sending: false,
    });
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle();
    assert!(trees.state().pending_finishes.contains_key("a"));
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, decomposed, pending)]);
    assert!(trees.state().pending_finishes["a"].sending);
}

#[test]
fn a_folder_the_disk_holds_at_its_owed_name_in_another_unicode_form_finishes_disk_first() {
    let (name, decomposed, pending) = ("\u{e9}", "e\u{301}", "\u{e9} (pending)");
    let mut trees = trees(&[(
        "a",
        Some(dir(ROOT, decomposed)),
        Some(dir(ROOT, pending)),
        Some(remote(dir(ROOT, pending))),
    )]);
    let marked = RemoteNode {
        pending_for: Some(name.into()),
        pending_by: Some("me".into()),
        ..remote(dir(ROOT, pending))
    };
    trees.set_remote("a".into(), marked);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, decomposed, pending)]);
    let finish = &trees.state().pending_finishes["a"];
    assert!(finish.disk_first && finish.sending, "{finish:?}");
}

#[test]
fn a_folder_the_user_moves_to_its_owed_name_in_another_unicode_form_finishes_disk_first() {
    let (name, decomposed, pending) = ("\u{e9}", "e\u{301}", "\u{e9} (pending)");
    let mut trees = trees(&[(
        "a",
        Some(dir(ROOT, pending)),
        Some(dir(ROOT, pending)),
        Some(remote(dir(ROOT, pending))),
    )]);
    let marked = RemoteNode {
        pending_for: Some(name.into()),
        pending_by: Some("me".into()),
        ..remote(dir(ROOT, pending))
    };
    trees.set_remote("a".into(), marked);
    let mut state = trees.state();
    state.pending_finishes.insert("a".into(), PendingFinish {
        parent: ROOT.into(),
        pending: pending.into(),
        name: name.into(),
        origin: FinishOrigin::Mark { meta_rev: 1 },
        disk_first: false,
        base: None,
        sending: false,
    });
    trees.restore(state).unwrap();
    trees.set_local("a".into(), local(dir(ROOT, decomposed), 1));
    assert!(trees.state().pending_finishes["a"].disk_first);
}

#[test]
fn a_folder_this_device_puts_back_at_its_pending_name_takes_the_name_the_server_gives_its_finish() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        (
            "a",
            Some(dir(ROOT, "a")),
            Some(dir(ROOT, "a (pending)")),
            Some(remote(dir(ROOT, "a (pending)"))),
        ),
        synced("x", file(ROOT, "A", 1)),
    ]);
    let mut state = trees.state();
    state.pending_finishes.insert("a".into(), PendingFinish {
        parent: ROOT.into(),
        pending: "a (pending)".into(),
        name: "a".into(),
        origin: FinishOrigin::Mark { meta_rev: 1 },
        disk_first: true,
        base: None,
        sending: false,
    });
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle();
    trees.revert("a".into());
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    let at = |trees: &Trees| (trees.local().get("a")).map(|local| local.shape.name.clone());
    assert_eq!(at(&trees).as_deref(), Some("a (pending)"));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    let renamed = OpResult {
        name: "a (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (conflict)"), 2));
    commits.received(result(2, vec![renamed])).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(at(&trees).as_deref(), Some("a (conflict)"));
    let next = build(&mut commits, &mut trees, &mut ids);
    assert_eq!(next.as_ref().map(ops), None);
}

#[test]
fn a_folder_another_device_deletes_takes_its_owed_name_once_a_parked_edit_below_it_brings_it_back() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        (
            "a",
            Some(dir(ROOT, "a")),
            Some(dir(ROOT, "a (pending)")),
            Some(trashed(dir(ROOT, "a (pending)"))),
        ),
        (
            "k",
            Some(file("a", "child", 1)),
            Some(file("a", "child", 1)),
            Some(trashed(file("a", "child", 1))),
        ),
        (
            "h",
            Some(file("a", "old", 1)),
            Some(file("a", "old", 1)),
            Some(trashed(file("a", "old", 1))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file("a", "child", 2), 7)
        },
    );
    let mut state = trees.state();
    state.pending_finishes.insert("a".into(), PendingFinish {
        parent: ROOT.into(),
        pending: "a (pending)".into(),
        name: "a".into(),
        origin: FinishOrigin::Mark { meta_rev: 1 },
        disk_first: true,
        base: None,
        sending: false,
    });
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle();
    assert!(!commits.acknowledge(&mut trees, 1));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (pending)"), 2));
    let applied = vec![outcome(op_result::Code::Applied); sent.ops.len()];
    commits.received(result(2, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.as_ref().is_some_and(|commit| ops(commit).contains(&owed_mv("a", ROOT, "a", "a (pending)"))),
        "{next:?}"
    );
}

#[test]
fn a_folder_another_device_deletes_takes_its_owed_name_once_an_unsent_edit_below_it_brings_it_back()
{
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        (
            "a",
            Some(dir(ROOT, "a")),
            Some(dir(ROOT, "a (pending)")),
            Some(trashed(dir(ROOT, "a (pending)"))),
        ),
        (
            "f",
            Some(file("a", "c.txt", 1)),
            Some(file("a", "c.txt", 1)),
            Some(trashed(file("a", "c.txt", 1))),
        ),
    ]);
    let mut state = trees.state();
    state.unsent_edits.insert("f".into());
    state.dirty.insert("f".into());
    state.pending_finishes.insert("a".into(), PendingFinish {
        parent: ROOT.into(),
        pending: "a (pending)".into(),
        name: "a".into(),
        origin: FinishOrigin::Mark { meta_rev: 1 },
        disk_first: true,
        base: None,
        sending: false,
    });
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle();
    assert!(!commits.acknowledge(&mut trees, 1));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        ops(&sent)
            .iter()
            .any(|op| matches!(op, op::Op::UpdateContent(update) if update.node_id == "f"))
    );
    assert!(
        !ops(&sent)
            .iter()
            .any(|op| matches!(op, op::Op::Move(moved) if moved.node_id == "a"))
    );
    let revived = RemoteNode {
        pending_for: Some("a".into()),
        pending_by: Some("me".into()),
        ..remote_at(dir(ROOT, "a (pending)"), 8)
    };
    trees.set_remote("a".into(), revived);
    trees.set_remote("f".into(), remote_at(file("a", "c.txt", 1), 8));
    commits
        .received(result(
            8,
            vec![outcome(op_result::Code::Applied); sent.ops.len()],
        ))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    let rename = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&rename), [owed_mv("a", ROOT, "a", "a (pending)")]);
}

#[test]
fn a_superseded_owed_rename_ends_the_debt_and_leaves_the_change_it_met_to_reach_the_disk() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    effects_for(&mut trees, &mut ids, "a");
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    trees.set_remote("a".into(), remote_at(dir(ROOT, "b"), 2));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Superseded)],
    );
    assert!(!trees.state().pending_finishes.contains_key("a"));
    assert_eq!(trees.synced().live("a"), Some(&dir(ROOT, "a")));
    assert_eq!(trees.class("a"), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_superseded_owed_rename_of_a_node_another_device_deleted_keeps_the_debt() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    effects_for(&mut trees, &mut ids, "a");
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let marked = |node: RemoteNode| RemoteNode {
        pending_for: Some("a".into()),
        pending_by: Some("me".into()),
        ..node
    };
    trees.set_remote("a".into(), marked(trashed(dir(ROOT, "a (pending)"))));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Superseded)],
    );
    assert!(!trees.state().pending_finishes.contains_key("a"));
    trees.set_remote("a".into(), marked(remote_at(dir(ROOT, "a (pending)"), 3)));
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    let rename = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&rename), [owed_mv("a", ROOT, "a", "a (pending)")]);
}

#[test]
fn a_folder_another_device_deletes_and_brings_back_at_its_pending_name_takes_its_own_name() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[(
        "a",
        Some(dir(ROOT, "a")),
        Some(dir(ROOT, "a (pending)")),
        Some(remote(dir(ROOT, "a (pending)"))),
    )]);
    let mut state = trees.state();
    state.pending_finishes.insert("a".into(), PendingFinish {
        parent: ROOT.into(),
        pending: "a (pending)".into(),
        name: "a".into(),
        origin: FinishOrigin::Mark { meta_rev: 1 },
        disk_first: true,
        base: None,
        sending: false,
    });
    trees.restore(state).unwrap();
    trees.rules_scanned();
    let marked = |node: RemoteNode| RemoteNode {
        pending_for: Some("a".into()),
        pending_by: Some("me".into()),
        ..node
    };
    trees.set_remote("a".into(), marked(trashed(dir(ROOT, "a (pending)"))));
    commits.acknowledge(&mut trees, 2);
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "a");
    assert!(trees.local().get("a").is_none());
    trees.set_remote("a".into(), marked(remote_at(dir(ROOT, "a (pending)"), 3)));
    commits.acknowledge(&mut trees, 3);
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    let rename = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&rename), [owed_mv("a", ROOT, "a", "a (pending)")]);
}

#[test]
fn s_takes_the_mark_r_gives_the_position_s_already_holds() {
    let (_, mut trees, _) = owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    assert_eq!(trees.synced_mark("a"), Some("a"));
    let cleared = RemoteNode {
        pending_for: None,
        pending_by: None,
        ..remote_at(dir(ROOT, "a (pending)"), 2)
    };
    trees.set_remote("a".into(), cleared);
    trees.settle();
    assert_eq!(trees.synced_mark("a"), None);
}

#[test]
fn a_size_refusal_leaves_the_finishes_of_its_commit_due() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("a", dir(ROOT, "a (pending)")),
        synced("b", dir(ROOT, "a")),
        synced("c", dir(ROOT, "c (pending)")),
        synced("d", dir(ROOT, "c")),
    ]);
    let mut other = trees.remote().get("c").unwrap().clone();
    other.pending_for = Some("c".into());
    other.pending_by = Some("me".into());
    trees.set_remote("c".into(), other);
    trees.settle();
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&first), [owed_mv("a", ROOT, "a", "a (pending)"), owed_mv("c", ROOT, "c", "c (pending)")]);
    commits
        .failed(&mut trees, &refusal(error::Code::LimitExceeded, 0))
        .unwrap();
    assert!(commits.due(&trees, "me"));
}

#[test]
fn a_finish_kept_through_a_rewind_takes_the_name_the_rewound_server_marks() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("a", dir(ROOT, "a (pending)")),
        synced("b", dir(ROOT, "a")),
        synced("c", dir(ROOT, "c")),
    ]);
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&first), [owed_mv("a", ROOT, "a", "a (pending)")]);
    let mut after_rewind = trees.remote().get("a").unwrap().clone();
    after_rewind.pending_for = Some("c".into());
    trees.set_remote("a".into(), after_rewind);
    commits.recover(
        &mut trees,
        0,
        &RecoveryCause::of(RecoveryTrigger::SpaceRewound),
        &mut ids,
        NOW,
    );
    trees.settle();
    let next = commits
        .build(&mut trees, &mut ids, NOW, "new-history", "me", 7)
        .cloned();
    assert_eq!(next.as_ref().map(ops), Some(vec![owed_mv("a", ROOT, "c", "a (pending)")]));
}

#[test]
fn a_folder_the_user_moves_while_it_is_parked_for_a_swap_drops_the_mark_it_kept() {
    let mut trees = trees(&[("n", Some(dir(ROOT, "a (pending)")), None, None)]);
    let mut state = trees.state();
    state.unsent_marks.insert("n".into(), "a".into());
    trees.restore(state).unwrap();
    trees.rules_scanned();
    let held = trees.local().get("n").unwrap().clone();
    trees.set_local(
        "n".into(),
        LocalNode {
            parked: true,
            ..held.clone()
        },
    );
    trees.set_local_changed(
        "n".into(),
        LocalNode {
            shape: dir(ROOT, "b"),
            parked: false,
            ..held
        },
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    let create = (ops(&sent).into_iter())
        .find_map(|op| match op {
            op::Op::CreateDir(create) if create.node_id == "n" => Some(create),
            _ => None,
        })
        .expect("n is created");
    assert_eq!((create.name.as_str(), create.pending_for.as_str()), ("b", ""));
}

#[test]
fn a_folder_created_under_a_temporary_name_keeps_the_name_it_was_pending_for() {
    let mut trees = trees(&[
        ("n", Some(dir(ROOT, "a (pending)")), None, None),
        (
            "m",
            Some(dir("n", "m")),
            Some(dir(ROOT, "a (pending)")),
            Some(remote(dir(ROOT, "a (pending)"))),
        ),
    ]);
    let mut state = trees.state();
    state.unsent_marks.insert("n".into(), "a".into());
    trees.restore(state).unwrap();
    trees.rules_scanned();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    let create = (ops(&sent).into_iter())
        .find_map(|op| match op {
            op::Op::CreateDir(create) if create.node_id == "n" => Some(create),
            _ => None,
        })
        .expect("n is created");
    assert_ne!(create.name, "a (pending)", "{sent:?}");
    assert_eq!(create.pending_for, "a", "{sent:?}");
}

#[test]
fn a_finish_below_a_rules_file_that_failed_to_download_waits_for_the_rules() {
    let (_, mut trees, mut ids) = marked_by(
        &[
            synced("p", dir(ROOT, "p")),
            synced("a", dir("p", "a (pending)")),
            ("r", None, None, Some(remote(file("p", ".koiignore", 4)))),
        ],
        "me",
    );
    let mut effects = Effects::default();
    let first = effects.plan(&trees, None, &mut ids, NOW, BatchLimit::ONE).first().cloned().unwrap();
    assert_eq!(first.node, "r");
    effects.failed(&mut trees, &first.effect_id).unwrap();
    trees.settle();
    let next = effects.plan(&trees, None, &mut ids, NOW, BatchLimit::ONE).first().cloned();
    assert!(next.as_ref().is_none_or(|record| record.node != "a"), "{next:?}");
}

#[test]
fn a_folder_named_like_the_pending_name_of_a_held_finish_is_created_under_a_pending_name() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(trees.local().live("a"), Some(&dir(ROOT, "a")));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [owed_mv("a", ROOT, "a", "a (pending)")]);
    commits
        .failed(&mut trees, &refusal(error::Code::HeadMoved, 0))
        .unwrap();
    trees.set_local("n".into(), local(dir(ROOT, "a (pending)"), 50));
    trees.settle();
    let held = build(&mut commits, &mut trees, &mut ids).unwrap();
    let created = proto::CreateDir {
        node_id: "n".into(),
        parent_id: ROOT.into(),
        name: "a (pending) (pending)".into(),
        pending_for: "a (pending)".into(),
    };
    assert_eq!(ops(&held), [op::Op::CreateDir(created)]);
    let marked = RemoteNode {
        pending_for: Some("a (pending)".into()),
        pending_by: Some("me".into()),
        ..remote_at(dir(ROOT, "a (pending) (pending)"), 8)
    };
    trees.set_remote("n".into(), marked);
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    let next = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 8)
        .cloned()
        .unwrap();
    assert_eq!(ops(&next).first(), Some(&owed_mv("a", ROOT, "a", "a (pending)")), "{next:?}");
}

fn an_owed_folder_deleted_while_an_edit_below_it_waits(marked: bool) -> (Commits, Trees, IdGen) {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "child", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        ("n", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let created = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(ops(&created).as_slice(), [op::Op::CreateDir(create), ..]
        if create.node_id == "n" && create.pending_for == "a"));
    trees.set_remote("n".into(), owed_at(marked, 8));
    trees.set_remote("h".into(), remote_at(file("n", "child", 1), 8));
    commits
        .received(result(
            8,
            vec![outcome(op_result::Code::Applied); created.ops.len()],
        ))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    trees.set_local("h".into(), local(file("n", "child", 2), 1));
    for id in ["n", "h"] {
        let deleted = RemoteNode {
            trashed: true,
            meta_rev: 9,
            trash_group: "9:0".into(),
            ..trees.remote().get(id).unwrap().clone()
        };
        trees.set_remote(id.into(), deleted);
    }
    trees.settle();
    (commits, trees, ids)
}

fn owed_at(marked: bool, rev: u64) -> RemoteNode {
    RemoteNode {
        pending_for: marked.then(|| "a".into()),
        pending_by: marked.then(|| "me".into()),
        ..remote_at(dir(ROOT, "a (pending)"), rev)
    }
}

fn revive(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> proto::Commit {
    let revive = (commits.build(trees, ids, NOW, HISTORY, "me", 9).cloned()).unwrap();
    assert!(
        !(ops(&revive).iter()).any(|op| matches!(op, op::Op::Move(moved) if moved.node_id == "n")),
        "{revive:?}"
    );
    assert!(
        (ops(&revive).iter())
            .any(|op| matches!(op, op::Op::UpdateContent(update) if update.node_id == "h")),
        "{revive:?}"
    );
    revive
}

fn acknowledge_revival(commits: &mut Commits, trees: &mut Trees, sent: &proto::Commit, seq: u64) {
    let child = RemoteNode {
        content_rev: seq,
        edit_rev: seq,
        ..remote_at(file("n", "child", 2), seq)
    };
    trees.set_remote("h".into(), child);
    let applied = vec![outcome(op_result::Code::Applied); sent.ops.len()];
    commits.received(result(seq, applied)).unwrap();
    assert!(commits.acknowledge(trees, seq));
    trees.settle();
}

#[test]
fn a_folder_an_edit_revives_takes_its_owed_name_from_a_server_that_keeps_marks() {
    a_folder_an_edit_revives_takes_its_owed_name(true);
}

#[test]
fn a_folder_an_edit_revives_takes_its_owed_name_from_a_server_that_keeps_no_marks() {
    a_folder_an_edit_revives_takes_its_owed_name(false);
}

fn a_folder_an_edit_revives_takes_its_owed_name(marked: bool) {
    let (mut commits, mut trees, mut ids) = an_owed_folder_deleted_while_an_edit_below_it_waits(marked);
    let sent = revive(&mut commits, &mut trees, &mut ids);
    trees.set_remote("n".into(), owed_at(marked, 10));
    acknowledge_revival(&mut commits, &mut trees, &sent, 10);
    effects_for(&mut trees, &mut ids, "n");
    let rename = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 10).cloned();
    assert_eq!(rename.as_ref().map(ops), Some(vec![owed_mv("n", ROOT, "a", "a (pending)")]));
}

#[test]
fn a_folder_an_edit_revives_takes_its_owed_name_once_the_held_edit_is_released() {
    let (mut commits, mut trees, mut ids) = an_owed_folder_deleted_while_an_edit_below_it_waits(false);
    trees.set_rules_unknown("n");
    let held = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 9).cloned();
    assert!(
        !held.as_ref().is_some_and(|commit| (ops(commit).iter())
            .any(|op| matches!(op, op::Op::UpdateContent(update) if update.node_id == "h"))),
        "{held:?}"
    );
    assert!(trees.state().pending_finishes.contains_key("n"));
    trees.set_rules_known("n");
    trees.settle();
    let sent = revive(&mut commits, &mut trees, &mut ids);
    trees.set_remote("n".into(), owed_at(false, 10));
    acknowledge_revival(&mut commits, &mut trees, &sent, 10);
    effects_for(&mut trees, &mut ids, "n");
    let rename = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 10).cloned();
    assert_eq!(rename.as_ref().map(ops), Some(vec![owed_mv("n", ROOT, "a", "a (pending)")]));
}

#[test]
fn an_owed_rename_ends_when_another_device_revives_the_folder_first() {
    let (mut commits, mut trees, mut ids) = an_owed_folder_deleted_while_an_edit_below_it_waits(false);
    let sent = revive(&mut commits, &mut trees, &mut ids);
    trees.set_remote("n".into(), owed_at(false, 10));
    acknowledge_revival(&mut commits, &mut trees, &sent, 11);
    assert!(!trees.state().pending_finishes.contains_key("n"));
    effects_for(&mut trees, &mut ids, "n");
    let next = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 11).cloned();
    assert!(
        !next.as_ref().is_some_and(|commit| ops(commit).contains(&mv("n", ROOT, "a"))),
        "{next:?}"
    );
}

#[test]
fn an_owed_rename_ends_when_the_user_deletes_the_folder_an_edit_would_revive() {
    let (mut commits, mut trees, mut ids) = an_owed_folder_deleted_while_an_edit_below_it_waits(false);
    trees.remove_local("h");
    trees.remove_local("n");
    trees.settle();
    assert!(!trees.state().pending_finishes.contains_key("n"));
    let next = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 9).cloned();
    assert_eq!(next.as_ref().map(ops), None);
}
