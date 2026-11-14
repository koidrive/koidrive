// SPDX-License-Identifier: Apache-2.0

/// The user moves n back into h. Its content agrees with the waiting shape, not with S, so S does
/// not take that content as both sides' and drop the waiting update, and the move is sent instead
/// of the effects undoing it.
#[test]
fn a_file_moved_back_into_a_trashed_folder_while_its_update_waits_stays_there() {
    let (mut trees, mut commits, mut ids) =
        a_file_moved_out_of_a_trashed_folder_whose_update_waits();
    trees.set_local("n".into(), local(file("h", "old", 1), 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", "h", "old")]);
    effects_for(&mut trees, &mut ids, "n");
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.local().live("n"), Some(&file("h", "old", 1)));
}

/// As above, with an edit of n on the other device and a move of n into h on this one. The move is
/// sent, so neither n's update nor h's removal waits for the other for good.
#[test]
fn a_file_moved_into_a_trashed_folder_while_its_update_waits_and_it_is_edited_remotely_is_sent() {
    let (mut trees, mut commits, mut ids) =
        a_file_moved_out_of_a_trashed_folder_whose_update_waits();
    trees.set_local("n".into(), local(file("h", "z", 1), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "X", 3)));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", "h", "z")]);
    assert_eq!(trees.check(), Ok(()));
}

/// The user puts n back where S still holds it, with the content S still holds, and the server
/// deletes n. Against the acknowledged update that is an edit, so it is uploaded, whether the
/// effects or a commit come first, and whether n is parked or not.
#[test]
fn a_waiting_file_put_back_as_s_still_holds_it_is_uploaded_after_a_remote_delete() {
    for (effects_first, parked) in [(true, false), (true, true), (false, false)] {
        let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
        trees.set_local(
            "n".into(),
            LocalNode {
                parked,
                ..local(file(ROOT, "old", 0), 2)
            },
        );
        trees.set_remote("n".into(), trashed(file(ROOT, "x", 1)));
        trees.settle();
        assert_eq!(trees.class("n"), Class::Remote, "L matches S");
        assert!(trees.edited("n"));
        if effects_first {
            let effects = effects_for(&mut trees, &mut ids, "n");
            assert!(
                effects
                    .iter()
                    .all(|effect| matches!(effect, Effect::Move { .. })),
                "{effects:?}"
            );
            assert_eq!(trees.local().get("n").map(|node| node.parked), Some(false));
            let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
            assert_eq!(ops(&commit), [mv("n", ROOT, "old"), update("n", 0)]);
            continue;
        }
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
            (ROOT, "old", Some(content(0)))
        );
        assert_eq!(effects_for(&mut trees, &mut ids, &create.node_id), []);
        assert_eq!(trees.local().get(&create.node_id).unwrap().identity.ino, 2);
    }
}

#[test]
fn a_waiting_update_both_sides_moved_past_keeps_its_content_as_the_base_of_a_later_edit() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "z", 0), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "z", 1)));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 0)]);
}

/// The other device moves n on and puts a new folder m where n's update would place it, while the
/// user moves n back. The update's position is stale, so it no longer holds n out of planning, and
/// n follows the remote move with the acknowledged content.
#[test]
fn a_waiting_update_whose_place_r_gave_away_does_not_hold_the_node_for_good() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "old", 1), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "q", 1)));
    trees.set_remote("m".into(), remote(dir(ROOT, "x")));
    trees.settle();
    for _ in 0..3 {
        effects_for(&mut trees, &mut ids, "");
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    }
    assert_eq!(trees.waiting(), &Default::default());
    assert_eq!(
        trees.local().get("n").map(|node| &node.shape),
        Some(&file(ROOT, "q", 1))
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "q", 1)));
    assert_eq!(trees.synced().live("m"), Some(&dir(ROOT, "x")));
}

/// n is parked as it was before the update, with the content S still holds, while R has n as the
/// update left it. Against the acknowledged update the old content is an edit, so n is only moved
/// out of the parked directory, and the content is uploaded once the update is made.
#[test]
fn a_parked_waiting_file_with_the_content_s_still_holds_keeps_it() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local(
        "n".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "old", 0), 2)
        },
    );
    trees.settle();
    assert_eq!(trees.class("n"), Class::Remote, "L matches S");
    let effects = effects_for(&mut trees, &mut ids, "n");
    assert!(
        effects
            .iter()
            .all(|effect| matches!(effect, Effect::Move { .. })),
        "{effects:?}"
    );
    assert_eq!(
        trees
            .local()
            .get("n")
            .map(|node| (&node.shape, node.parked)),
        Some((&file(ROOT, "x", 0), false))
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 0)]);
}

/// n is parked with the content the server acknowledged, which its waiting update holds, while
/// the other device moves n to q and changes its content, and committing is halted. Against the
/// acknowledged update only R changed, so the effects write R's content at q, although no commit
/// rebases n while committing is halted.
#[test]
fn a_parked_waiting_file_takes_remote_content_while_committing_is_halted() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local(
        "n".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 1), 2)
        },
    );
    trees.set_remote("n".into(), remote(file(ROOT, "q", 3)));
    trees.set_local("p".into(), local(file(ROOT, "p", 3), 3));
    trees.settle();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        commits.failed(&mut trees, &refusal(error::Code::ReadOnly, 0)),
        Ok(Failure::Halted(Halt::ReadOnly))
    );
    effects_for(&mut trees, &mut ids, "n");
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "n");
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "q", 3)));
}

/// As above, but the user edited n while it was parked. That edit is the user's, so n is only
/// moved to q, with the edited content, and the edit is uploaded as a conflict copy beside R's.
#[test]
fn a_parked_waiting_file_the_user_edited_keeps_the_edit_when_the_other_device_moves_it() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local(
        "n".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 2), 2)
        },
    );
    trees.set_remote("n".into(), remote(file(ROOT, "q", 3)));
    trees.settle();
    let effects = effects_for(&mut trees, &mut ids, "n");
    assert!(
        effects
            .iter()
            .all(|effect| matches!(effect, Effect::Move { .. })),
        "{effects:?}"
    );
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "q", 2)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let [op::Op::CreateFile(copy)] = <[op::Op; 1]>::try_from(ops(&commit)).unwrap() else {
        panic!("{commit:?}");
    };
    assert_eq!(
        (copy.parent_id.as_str(), copy.content),
        (ROOT, Some(content(2)))
    );
}

/// Both sides move n to z with the same new content while its update waits. Settling makes S
/// take that shape, which replaces the update, so nothing is uploaded and nothing is forked.
#[test]
fn a_waiting_update_settling_replaced_is_not_made_afterwards() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "z", 2), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "z", 2)));
    trees.settle();
    assert_eq!(trees.waiting(), &Default::default());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "z", 2)));
}

/// The server renames n's move, so n is reverted as well as waiting, and then the user puts n back
/// as S still holds it. That move is the user's last word, so n is not moved to its R name, and
/// its content, an edit against the acknowledged update, is neither replaced nor removed. Both are
/// uploaded once the update is made.
#[test]
fn a_renamed_waiting_file_put_back_as_s_still_holds_it_keeps_its_content() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x (2)", 9)),
            Some(file(ROOT, "x (2)", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        (
            "n",
            Some(file(ROOT, "x", 1)),
            Some(file(ROOT, "old", 0)),
            Some(remote(file(ROOT, "old", 0))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x (2)", 9), 1)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
    trees.set_remote("n".into(), remote(file(ROOT, "x (2)", 1)));
    trees.set_remote("z".into(), remote(file(ROOT, "x", 8)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![
            OpResult {
                name: "x (2)".into(),
                ..outcome(op_result::Code::AppliedRenamed)
            },
            outcome(op_result::Code::Applied),
        ],
    );
    assert!(trees.waiting().contains_key("n") && trees.reverted().contains("n"));
    trees.set_local("n".into(), local(file(ROOT, "old", 0), 2));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Remote, "L matches S");
    assert!(!trees.reverted().contains("n"));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(
        trees.local().get("n").map(|node| &node.shape),
        Some(&file(ROOT, "old", 0))
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "old"), update("n", 0)]);
}

/// n's update waits and h needs n's place on disk, so n is parked. When the driver fails to park
/// it, the same move is not planned again until something changes.
#[test]
fn a_waiting_blocker_whose_parking_failed_is_not_parked_again_unchanged() {
    let (mut trees, _, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "old", 0), 2));
    trees.set_remote("h".into(), remote(file(ROOT, "old", 9)));
    trees.settle();
    let mut effects = Effects::default();
    let first = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert_eq!(first.node, "n");
    assert!(matches!(
        first.effect,
        Effect::Move {
            to: Location::Parked(_),
            ..
        }
    ));
    effects.failed(&mut trees, &first.effect_id).unwrap();
    assert_eq!(
        effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first(),
        None
    );
    assert!(effects.blocked().contains_key("n"));
}

/// Committing is halted, read-only, while n's update waits. Once the update fits, building makes it
/// anyway, so S stays in step while downloads go on.
#[test]
fn a_waiting_update_is_made_while_committing_is_halted() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("p".into(), local(file(ROOT, "p", 3), 3));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "p"));
    assert_eq!(
        commits.failed(&mut trees, &refusal(error::Code::ReadOnly, 0)),
        Ok(Failure::Halted(Halt::ReadOnly))
    );
    effects_for(&mut trees, &mut ids, "");
    assert_eq!(trees.synced().live("h"), Some(&file(ROOT, "y", 9)));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.waiting(), &Default::default());
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "x", 1)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
}

/// As above, with an edit the user made after the commit: L leaves the waiting shape, so nothing
/// but building makes the update. The edit is planned against the waiting shape and goes out with
/// the commit that is refused. Once the update fits, building makes it while committing is halted,
/// and the edit stays the user's.
#[test]
fn a_waiting_update_is_made_while_committing_is_halted_under_a_local_edit() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "x", 2), 2));
    trees.set_local("p".into(), local(file(ROOT, "p", 3), 3));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::UpdateContent(u), op::Op::CreateFile(c)] if u.node_id == "n" && c.node_id == "p"),
        "{:?}",
        ops(&commit)
    );
    assert_eq!(
        commits.failed(&mut trees, &refusal(error::Code::ReadOnly, 0)),
        Ok(Failure::Halted(Halt::ReadOnly))
    );
    effects_for(&mut trees, &mut ids, "");
    assert_eq!(trees.synced().live("h"), Some(&file(ROOT, "y", 9)));
    assert!(trees.waiting().contains_key("n"));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(trees.waiting().is_empty());
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "x", 1)));
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "x", 2)));
}

/// The other device moves n on while n's update waits behind h. Against the waiting shape that is
/// a remote change, so the effects move n at once, and the update then takes R's position.
#[test]
fn a_remote_move_of_a_node_whose_update_waits_reaches_the_disk() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_remote("n".into(), remote(file(ROOT, "q", 1)));
    trees.settle();
    let planned = effects_for(&mut trees, &mut ids, "n");
    assert!(matches!(&planned[..], [Effect::Move { .. }]), "{planned:?}");
    assert_eq!(
        trees.local().get("n").map(|node| &node.shape),
        Some(&file(ROOT, "q", 1))
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.waiting(), &Default::default());
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "q", 1)));
}

/// The user creates folder d at X, where S still holds parked file h at x, and file m in d, so the
/// updates of both wait. The other device moves h into d, and the user deletes d with m. d is
/// brought back for h, and m's delete is uploaded once its update fits.
#[test]
fn a_waiting_folder_the_user_deleted_is_brought_back_for_a_remote_move_into_it() {
    let mut trees = trees(&[
        ("d", Some(dir(ROOT, "X")), None, None),
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        ("m", Some(file("d", "c", 1)), None, None),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 2)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), 2);
    trees.set_remote("d".into(), remote(dir(ROOT, "X")));
    trees.set_remote("m".into(), remote(file("d", "c", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    assert!(trees.waiting().contains_key("d") && trees.waiting().contains_key("m"));
    trees.set_remote("h".into(), remote(file("d", "h", 9)));
    trees.remove_local("m");
    trees.remove_local("d");
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "");
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("m")]);
    assert_eq!(
        trees.local().get("d").map(|node| &node.shape),
        Some(&dir(ROOT, "X"))
    );
    let h = trees.local().get("h").unwrap();
    assert_eq!((&h.shape, h.parked), (&file("d", "h", 9), false));
}

/// GUN-350. The user moves f out of folder d while the other device removes both. The removal of
/// the empty d is in flight when R restores d with f in it. S cannot drop d while it holds f, so
/// that update waits instead of being lost. The commit sends only the user's move, and once it is
/// acknowledged S drops d, the effects bring d back, and the trees end in sync.
#[test]
fn a_removal_r_undid_whose_s_update_does_not_fit_yet_waits() {
    let mut trees = trees(&[
        ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d")), None),
        ("f", Some(file(ROOT, "f", 1)), Some(file("d", "f", 1)), None),
    ]);
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let record = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert_eq!(record.node, "d");
    assert!(
        matches!(record.effect, Effect::RemoveDir { .. }),
        "{record:?}"
    );
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    trees.set_remote("f".into(), remote(file("d", "f", 1)));
    effects
        .done(&mut trees, None, &record.effect_id, None)
        .unwrap();
    trees.settle();

    assert_eq!(effects_for(&mut trees, &mut ids, "d"), []);

    let mut commits = Commits::default();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", ROOT, "f")]);
    trees.set_remote("f".into(), remote(file(ROOT, "f", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.waiting(), &Default::default());
    let effects = effects_for(&mut trees, &mut ids, "d");
    assert!(
        matches!(effects[..], [Effect::CreateDir { .. }]),
        "{effects:?}"
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.class("d"), Class::InSync);
}

/// S still holds parked h at x when d's move to X is acknowledged, so d's update waits, and S
/// still holds m in d. The user deletes d and parks m, and the other device moves h into d. The
/// keep rule needs d but cannot clear it from S yet. d's acknowledged move keeps waiting, so the
/// effects bring d back at X for h.
#[test]
fn a_needed_folder_whose_move_waits_is_brought_back() {
    let mut trees = trees(&[
        (
            "d",
            Some(dir(ROOT, "X")),
            Some(dir(ROOT, "old")),
            Some(remote(dir(ROOT, "old"))),
        ),
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        synced("m", file("d", "c", 1)),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 2)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("d", ROOT, "X")]);
    trees.set_remote("d".into(), remote(dir(ROOT, "X")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(
        trees.waiting().get("d").map(|waiting| &waiting.shape),
        Some(&Some(dir(ROOT, "X")))
    );
    trees.set_local(
        "m".into(),
        LocalNode {
            parked: true,
            ..local(file("d", "c", 1), 3)
        },
    );
    trees.remove_local("d");
    trees.set_remote("h".into(), remote(file("d", "h", 9)));
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    let effects = effects_for(&mut trees, &mut ids, "d");
    assert!(
        matches!(effects[..], [Effect::CreateDir { .. }]),
        "{effects:?}"
    );
    assert_eq!(trees.local().live("d"), Some(&dir(ROOT, "X")));
}

#[test]
fn a_removal_that_waits_after_both_sides_dropped_the_node_is_kept_until_it_fits() {
    let mut trees = trees(&[
        ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d")), None),
        ("f", Some(file(ROOT, "f", 1)), Some(file("d", "f", 1)), None),
    ]);
    let (mut effects, mut ids, mut commits) =
        (Effects::default(), IdGen::new(1), Commits::default());
    assert_eq!(
        commits.connected(&Welcome {
            read_only: true,
            ..welcome(0)
        }),
        Reconnect::Idle
    );
    let record = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert_eq!(record.node, "d");
    assert!(matches!(record.effect, Effect::RemoveDir { .. }));
    effects
        .done(&mut trees, commits.in_flight(), &record.effect_id, None)
        .unwrap();
    trees.settle();
    assert_eq!(
        trees.waiting().get("d").map(|waiting| &waiting.shape),
        Some(&None)
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    trees.set_remote("f".into(), remote(file("d", "f", 1)));
    trees.settle();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Idle);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", ROOT, "f")]);
}

#[test]
fn a_kept_removal_is_made_once_s_no_longer_holds_anything_under_it() {
    let mut trees = trees(&[
        ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d")), None),
        ("f", Some(file(ROOT, "f", 1)), Some(file("d", "f", 1)), None),
    ]);
    let (mut effects, mut ids, mut commits) =
        (Effects::default(), IdGen::new(1), Commits::default());
    assert_eq!(
        commits.connected(&Welcome {
            read_only: true,
            ..welcome(0)
        }),
        Reconnect::Idle
    );
    let record = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    effects
        .done(&mut trees, commits.in_flight(), &record.effect_id, None)
        .unwrap();
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Idle);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(ops(&commit)[..], [op::Op::CreateFile(_)]));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(trees.waiting().is_empty());
    assert_eq!(trees.synced().live("d"), None);
}

#[test]
fn a_folder_delete_waits_while_a_resent_answer_delete_below_it_requires_the_original() {
    let mut trees = trees(&[
        synced("f", file(ROOT, "f", 1)),
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        (
            "y",
            None,
            Some(file("d", "y", 2)),
            Some(remote(file("d", "y", 2))),
        ),
    ]);
    let mut state = trees.state();
    state.kept_originals.insert("y".into(), "f".into());
    trees.restore(state).unwrap();
    trees.set_rules::<&str>(&[], None);
    trees.rules_scanned();
    let held = trees.remote().get("f").unwrap();
    let required = delete_copy_requiring(held.content_rev, held.meta_rev);
    let commit = build(&mut Commits::default(), &mut trees, &mut IdGen::new(1)).unwrap();
    assert_eq!(ops(&commit), [required]);
}

fn a_superseded_folder_delete_behind(h: LocalNode, moved_to: &str) {
    let h_shape = h.shape.clone();
    let mut trees = trees(&[
        ("d", None, Some(dir(ROOT, "d")), Some(remote(dir(ROOT, "d")))),
        (
            "c",
            Some(file(ROOT, moved_to, 1)),
            Some(file("d", "c", 1)),
            Some(remote(file("d", "c", 1))),
        ),
        (
            "h",
            Some(h_shape.clone()),
            Some(h_shape.clone()),
            Some(remote(h_shape)),
        ),
    ]);
    trees.set_local("h".into(), h);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let mut sent = ops(&commit);
    sent.sort_by_key(|op| matches!(op, op::Op::Delete(_)));
    assert_eq!(sent, [mv("c", ROOT, moved_to), delete("d")]);
    let results = (ops(&commit).iter())
        .map(|op| match op {
            op::Op::Delete(_) => outcome(op_result::Code::Superseded),
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    trees.set_remote("c".into(), remote(file(ROOT, moved_to, 1)));
    trees.set_remote("h".into(), remote(file("d", "h", 9)));
    acknowledge(&mut commits, &mut trees, results);
    assert_eq!(
        trees.waiting().keys().collect::<Vec<_>>(),
        ["c", "d"]
    );
    d_comes_back_for_h(&mut commits, &mut trees, &mut ids);
}

fn d_comes_back_for_h(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) {
    assert_eq!(build(commits, trees, ids), None);
    let effects = effects_for(trees, ids, "d");
    assert!(
        matches!(effects[..], [Effect::CreateDir { .. }]),
        "{effects:?}"
    );
    assert_eq!(trees.waiting(), &Default::default());
    let h = trees.local().get("h").unwrap();
    assert_eq!((&h.shape, h.parked), (&file("d", "h", 9), false));
    assert_eq!(build(commits, trees, ids), None);
    for id in ["d", "c", "h"] {
        assert_eq!(trees.class(id), Class::InSync, "{id}");
    }
}

#[test]
fn a_superseded_folder_delete_behind_a_name_that_differs_only_in_case_is_brought_back() {
    a_superseded_folder_delete_behind(local(file(ROOT, "C.TXT", 9), 3), "c.txt");
}

#[test]
fn a_superseded_folder_delete_behind_a_parked_file_is_brought_back() {
    a_superseded_folder_delete_behind(
        LocalNode {
            parked: true,
            ..local(file(ROOT, "y", 9), 3)
        },
        "y",
    );
}

#[test]
fn a_removed_folder_r_restored_behind_a_name_that_differs_only_in_case_is_brought_back() {
    let mut trees = trees(&[
        ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d")), None),
        ("c", Some(file(ROOT, "c.txt", 1)), Some(file("d", "c", 1)), None),
        synced("h", file(ROOT, "C.TXT", 9)),
    ]);
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let record = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert!(
        matches!(record.effect, Effect::RemoveDir { .. }),
        "{record:?}"
    );
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    trees.set_remote("c".into(), remote(file("d", "c", 1)));
    trees.set_remote("h".into(), remote(file("d", "h", 9)));
    effects
        .done(&mut trees, None, &record.effect_id, None)
        .unwrap();
    trees.settle();
    let mut commits = Commits::default();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("c", ROOT, "c.txt")]);
    trees.set_remote("c".into(), remote(file(ROOT, "c.txt", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(
        trees.waiting().keys().collect::<Vec<_>>(),
        ["c", "d"]
    );
    d_comes_back_for_h(&mut commits, &mut trees, &mut ids);
}
