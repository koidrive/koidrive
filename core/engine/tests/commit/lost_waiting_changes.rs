// SPDX-License-Identifier: Apache-2.0

/// The user moves and edits n again while its update waits. Compared with the waiting shape, only
/// L changed, so both ops are sent for n itself, which is not forked into a new file.
#[test]
fn a_node_whose_update_waits_is_moved_and_edited_again_under_its_id() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "z", 2), 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "z"), update("n", 2)]);
    assert!(trees.local().get("n").is_some());
}

/// The user takes n back to its S shape while the update to x waits. That is a change of L
/// against the waiting shape, not a remote change, so it is sent.
#[test]
fn a_node_whose_update_waits_taken_back_to_its_s_shape_is_sent() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "old", 0), 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "old"), update("n", 0)]);
}

/// The other device edits n while its update waits, and the user moves it. The edit does not hold
/// back the move (R4).
#[test]
fn a_remote_edit_of_a_node_whose_update_waits_does_not_hold_back_its_local_move() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "z", 1), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "x", 3)));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "z")]);
}

/// Once h leaves x, the waiting update could be made, and R's rollback would read as a remote
/// change that moves n back and writes content 0 over the edit. The update is dropped instead, so
/// the move and the edit are sent again.
#[test]
fn a_move_and_edit_the_server_lost_while_its_s_update_waits_is_sent_again() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits_lost_at_5();
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
}

/// As in `a_move_and_edit_whose_s_update_waits`, but the server loses every commit of this device,
/// and a bootstrap lists n at root/old with content 0 of seq 1, a seq the lost commit had. The
/// update came from the lost commit, so it is dropped. R holds n as S does, so R's content is not
/// written over the edit: the move and the edit are sent again.
#[test]
fn a_move_and_edit_whose_s_update_waits_is_not_overwritten_by_a_change_after_the_loss() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), remote(file(ROOT, "old", 0)));
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
}

/// A parked file h holds root/x in S while R has it at y, and the user creates n at x. The server
/// applies the create, but S cannot take n at x, so its update waits. The server then loses the
/// commit, and a bootstrap lists no n. Once h leaves x, the waiting update would put n in S, and
/// R's rollback would read as a remote delete of the only copy. The update is dropped instead, so
/// n is created again.
#[test]
fn a_create_the_server_lost_while_its_s_update_waits_is_sent_again() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "x", 9)),
        Some(file(ROOT, "x", 9)),
        Some(remote(file(ROOT, "y", 9))),
    )]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote(file(ROOT, "x", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert!(trees.waiting().contains_key("n"));
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.remove_remote("n");
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
}

#[test]
fn an_edit_to_the_content_a_lost_commit_replaced_is_sent_after_the_loss() {
    let (mut trees, mut commits, mut ids) = an_edit_to_the_content_a_lost_commit_replaced();
    assert_eq!(trees.class("n"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
    let sent = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        ..remote(file(ROOT, "n", 2))
    };
    trees.set_remote("n".into(), sent);
    let applied = vec![outcome(op_result::Code::Applied)];
    commits.received(result(3, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.class("n"), Class::InSync);
    assert!(trees.unsent_edits().is_empty());
}

/// content.
#[test]
fn a_remote_edit_before_an_edit_to_the_content_a_lost_commit_replaced_is_sent_keeps_both() {
    let (mut trees, mut commits, mut ids) = an_edit_to_the_content_a_lost_commit_replaced();
    let resent = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        ..remote(file(ROOT, "n", 3))
    };
    trees.set_remote("n".into(), resent);
    trees.settle();
    assert_eq!(trees.class("n"), Class::Concurrent);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id != "n" && c.content == Some(content(2))),
        "{:?}",
        ops(&commit)
    );
    assert_eq!(trees.class("n"), Class::Remote);
}

#[test]
fn an_edit_of_a_file_the_server_went_back_to_holding_in_the_trash_is_kept_from_a_resend() {
    let edited = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        ..remote(file(ROOT, "n", 3))
    };
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "n", 3)),
        Some(file(ROOT, "n", 3)),
        Some(edited),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 1));
    trees.settle();
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
    assert_eq!(trees.unsent_edits().iter().collect::<Vec<_>>(), ["n"]);
    trees.set_remote("n".into(), remote_at(file(ROOT, "n", 2), 3));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    let resent = RemoteNode {
        content_rev: 4,
        edit_rev: 4,
        ..remote_at(file(ROOT, "n", 3), 3)
    };
    trees.set_remote("n".into(), resent);
    trees.settle();
    assert_eq!(trees.class("n"), Class::Concurrent);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id != "n" && c.content == Some(content(2))),
        "{:?}",
        ops(&commit)
    );
    assert_eq!(trees.class("n"), Class::Remote);
}

#[test]
fn a_file_in_the_trash_after_a_loss_that_l_did_not_edit_is_not_recorded() {
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
    assert_eq!(trees.class("n"), Class::Remote);
}

#[test]
fn an_edit_this_device_committed_to_the_content_a_lost_commit_replaced_is_sent_again() {
    let edited = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        ..remote(file(ROOT, "n", 3))
    };
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "n", 3)),
        Some(file(ROOT, "n", 3)),
        Some(edited),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
    let applied = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        ..remote(file(ROOT, "n", 2))
    };
    trees.set_remote("n".into(), applied);
    let results = vec![outcome(op_result::Code::Applied)];
    commits.received(result(3, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.class("n"), Class::InSync);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), remote(file(ROOT, "n", 2)));
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
}

#[test]
fn a_write_to_the_content_the_server_went_back_to_seen_after_the_bootstrap_is_sent() {
    let edited = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        ..remote(file(ROOT, "n", 3))
    };
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "n", 3)),
        Some(file(ROOT, "n", 3)),
        Some(edited),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), remote(file(ROOT, "n", 2)));
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 1));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
}

#[test]
fn an_edit_sent_after_a_loss_and_lost_in_a_second_one_is_sent_again() {
    let (mut trees, mut commits, mut ids) = an_edit_to_the_content_a_lost_commit_replaced();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
    let applied = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        ..remote(file(ROOT, "n", 2))
    };
    trees.set_remote("n".into(), applied);
    let results = vec![outcome(op_result::Code::Applied)];
    commits.received(result(3, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.class("n"), Class::InSync);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), remote(file(ROOT, "n", 2)));
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
}

#[test]
fn an_edit_the_server_kept_is_not_sent_again_after_a_loss() {
    let mut trees = trees(&[synced("n", file(ROOT, "n", 3))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 1));
    trees.settle();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let applied = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        ..remote(file(ROOT, "n", 2))
    };
    trees.set_remote("n".into(), applied);
    let results = vec![outcome(op_result::Code::Applied)];
    commits.received(result(2, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    commit_file(&mut trees, &mut commits, &mut ids, "p", 3);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    assert_eq!(commits.held_through(), 2);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.remove_remote("p");
    trees.forget_lost(
        trashed,
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
    assert_eq!(trees.class("n"), Class::InSync);
}

#[test]
fn an_edit_back_to_the_recorded_content_after_another_content_is_still_sent() {
    let (mut trees, mut commits, mut ids) = an_edit_to_the_content_a_lost_commit_replaced();
    trees.set_local("n".into(), local(file(ROOT, "n", 4), 1));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 1));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    assert!(trees.edited("n"));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
}

#[test]
fn a_recorded_file_that_l_no_longer_holds_is_no_longer_recorded() {
    let (mut trees, _, _) = an_edit_to_the_content_a_lost_commit_replaced();
    assert_eq!(trees.unsent_edits().iter().collect::<Vec<_>>(), ["n"]);
    trees.remove_local("n");
    trees.settle();
    assert!(trees.unsent_edits().is_empty());
}

#[test]
fn a_recorded_file_another_device_purges_is_created_again() {
    let (mut trees, mut commits, mut ids) = an_edit_to_the_content_a_lost_commit_replaced();
    trees.remove_remote("n");
    trees.settle();
    let (mut effects, mut effect_ids) = (Effects::default(), IdGen::new(50));
    assert!(
        effects
            .plan(
                &trees,
                commits.in_flight(),
                &mut effect_ids,
                NOW,
                BatchLimit::ONE
            )
            .is_empty()
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n" && c.content == Some(content(2))),
        "{:?}",
        ops(&commit)
    );
    trees.set_remote("n".into(), remote_at(file(ROOT, "n", 2), 3));
    let results = vec![outcome(op_result::Code::Applied)];
    commits.received(result(3, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert!(trees.unsent_edits().is_empty());
    assert_eq!(trees.class("n"), Class::InSync);
}

#[test]
fn a_recorded_file_whose_s_update_waits_gets_no_download() {
    let restored = |unsent: bool| {
        let edited = RemoteNode {
            content_rev: 2,
            edit_rev: 2,
            ..remote(file(ROOT, "n", 3))
        };
        let mut trees = Trees::new(ROOT);
        trees.set_remote("n".into(), edited);
        trees.set_local("n".into(), local(file(ROOT, "n", 2), 1));
        trees.set_synced("n".into(), Some(file(ROOT, "n", 3)));
        let state = TreesState {
            dirty: ["n".into()].into(),
            waiting: [(
                "n".into(),
                Waiting {
                    shape: Some(file(ROOT, "n", 2)),
                    origin: Origin::Other,
                },
            )]
            .into(),
            unsent_edits: if unsent {
                ["n".into()].into()
            } else {
                Default::default()
            },
            ..Default::default()
        };
        trees.restore(state).unwrap();
        trees.rules_scanned();
        trees.settle();
        trees
    };
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    assert!(
        !effects
            .plan(&restored(false), None, &mut ids, NOW, BatchLimit::ONE)
            .is_empty()
    );
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    assert!(
        effects
            .plan(&restored(true), None, &mut ids, NOW, BatchLimit::ONE)
            .is_empty()
    );
}

/// GUN-418: the move and edit of n at slot 1 wait behind h. Another device moves n back at seq 2,
/// this device's commit at slot 2 and seq 3 is kept, and the one at slot 3 is lost. The bootstrap
/// lists n as S holds it. The update came from a kept commit, so it stays, and once h leaves x, n
/// takes the other device's change: nothing is sent for n.
#[test]
fn a_kept_move_that_another_device_reverses_before_a_loss_is_not_sent_again() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    let reversed = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        ..remote(file(ROOT, "old", 0))
    };
    trees.set_remote("n".into(), reversed);
    trees.settle();
    commit_file(&mut trees, &mut commits, &mut ids, "p", 3);
    commit_file(&mut trees, &mut commits, &mut ids, "q", 4);
    assert_eq!(commits.connected(&welcome(2)), Reconnect::AheadOfServer);
    assert_eq!(commits.held_through(), 3);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.remove_remote("q");
    trees.forget_lost(
        gone,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert!(trees.waiting().contains_key("n"));
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    let sent =
        build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    assert!(
        !sent.contains(&mv("n", ROOT, "x")) && !uploads(&sent, 1),
        "{sent:?}"
    );
    assert_eq!(trees.class("n"), Class::Remote);
}

/// GUN-418: the create of n at slot 1 waits behind h, and the commit at slot 2 is lost. Another
/// device then deletes n and the trash is purged, so the bootstrap lists no n. The create was
/// kept, so its update stays, and once h leaves x, n is a remote delete instead of being created
/// again.
#[test]
fn a_kept_create_that_another_device_deletes_after_a_loss_stays_deleted() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "x", 9)),
        Some(file(ROOT, "x", 9)),
        Some(remote(file(ROOT, "y", 9))),
    )]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("n".into(), remote(file(ROOT, "x", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert!(trees.waiting().contains_key("n"));
    commit_file(&mut trees, &mut commits, &mut ids, "p", 2);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.remove_remote("n");
    trees.remove_remote("p");
    trees.forget_lost(
        gone,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert!(trees.waiting().contains_key("n"));
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    let sent =
        build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    assert!(
        !sent
            .iter()
            .any(|op| matches!(op, op::Op::CreateFile(c) if c.node_id == "n")),
        "{sent:?}"
    );
    assert_eq!(trees.class("n"), Class::Remote);
}

/// GUN-418: the move and edit of n at slot 2 wait behind h. Another device moves n to root/z,
/// which file k holds in S while both sides move k away, and the effect gives L R's position. S
/// cannot take it yet, so the update waits again, and it keeps slot 2.
/// The server then loses slot 2, so the update is dropped, and the move and the edit are sent
/// again instead of R's rollback content being written over the edit.
#[test]
fn an_effect_on_a_node_whose_update_waits_keeps_its_slot_for_a_loss() {
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
        (
            "k",
            Some(file(ROOT, "z", 8)),
            Some(file(ROOT, "z", 8)),
            Some(remote(file(ROOT, "z", 8))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "p", 2);
    trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
    let moved = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        meta_rev: 5,
        meta_edit_rev: 5,
        ..remote(file(ROOT, "x", 1))
    };
    trees.set_remote("n".into(), moved);
    let applied = vec![outcome(op_result::Code::Applied); 2];
    commits.received(result(5, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(trees.waiting()["n"].origin, Origin::Commit(2));
    trees.set_local("k".into(), local(file(ROOT, "z2", 8), 3));
    let k_moved = RemoteNode {
        meta_rev: 6,
        meta_edit_rev: 6,
        ..remote(file(ROOT, "w", 8))
    };
    trees.set_remote("k".into(), k_moved);
    let moved_again = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        meta_rev: 7,
        meta_edit_rev: 7,
        ..remote(file(ROOT, "z", 1))
    };
    trees.set_remote("n".into(), moved_again);
    trees.settle();
    assert!(!effects_for(&mut trees, &mut ids, "n").is_empty());
    trees.settle();
    assert_eq!(
        trees
            .waiting()
            .get("n")
            .map(|waiting| (&waiting.shape, waiting.origin)),
        Some((&Some(file(ROOT, "z", 1)), Origin::Commit(2)))
    );
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), remote(file(ROOT, "old", 0)));
    trees.set_remote("k".into(), remote(file(ROOT, "z", 8)));
    trees.forget_lost(
        gone,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let sent =
        build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    assert!(
        sent.contains(&mv("n", ROOT, "z")) && sent.contains(&update("n", 1)),
        "{sent:?}"
    );
}

/// GUN-418: a waiting update taken by a rebase and waited again keeps the slot of its commit.
#[test]
fn a_waiting_update_keeps_its_slot_through_a_rebase() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    assert_eq!(trees.waiting()["n"].origin, Origin::Commit(1));
    trees.set_local("p".into(), local(file(ROOT, "p", 7), 50));
    trees.settle();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(trees.waiting()["n"].origin, Origin::Commit(1));
}

/// GUN-418: another device restores n with other content, and h leaves x, where the lost update
/// would fit. S never takes it, so L's edit, which only L holds, is uploaded rather than replaced
/// by R's content.
#[test]
fn a_lost_update_is_not_made_when_another_device_restores_the_node() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_lost_while_r_holds_it_in_the_trash();
    let restored = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        ..remote(file(ROOT, "old", 7))
    };
    trees.set_remote("n".into(), restored);
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    let sent =
        build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    assert!(!trees.waiting().contains_key("n"));
    assert!(uploads(&sent, 1), "{sent:?}");
}

/// GUN-418: as above, but h stays at x in R and S, so the lost update could never fit, and the
/// device is read-only, so nothing is sent. Another device restores n with other content. The
/// effects compare L with S, not with the lost shape, so n is not read as changed only remotely,
/// and L's edit is not replaced by R's content.
#[test]
fn a_lost_update_is_not_the_effects_base_while_it_cannot_fit() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("h".into(), remote(file(ROOT, "x", 9)));
    trees.set_remote("n".into(), trashed(file(ROOT, "old", 0)));
    trees.forget_lost(
        gone,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let mut state = commits.state();
    state.halted = Some(Halt::ReadOnly);
    let mut commits = Commits::from_state(state).unwrap();
    let restored = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        ..remote(file(ROOT, "old", 7))
    };
    trees.set_remote("n".into(), restored);
    trees.settle();
    let mut effects = Effects::default();
    let identity = FileIdentity {
        dev: 1,
        ino: 100,
        birth_ns: Some(0),
    };
    for _ in 0..3 {
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
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
    assert_eq!(
        trees.local().get("n").map(|node| &node.shape.content),
        Some(&Some(content(1)))
    );
}

/// GUN-418: this device moves file m out of folder d to root/x and deletes d. A parked file h
/// holds root/x in S while R has it at y, so m's update waits, S still holds m under d, and the
/// removal of d waits too. The server loses the commit and lists d and m as S holds them. Both
/// updates are dropped, so once h leaves x, the move and the delete are sent again instead of R's
/// d reading as a remote create.
#[test]
fn a_removal_the_server_lost_while_it_waits_is_sent_again() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        (
            "m",
            Some(file(ROOT, "x", 1)),
            Some(file("d", "m", 1)),
            Some(remote(file("d", "m", 1))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let applied = vec![outcome(op_result::Code::Applied); ops(&commit).len()];
    trees.remove_remote("d");
    trees.set_remote("m".into(), remote(file(ROOT, "x", 1)));
    acknowledge(&mut commits, &mut trees, applied);
    assert_eq!(
        trees.waiting().get("d").map(|waiting| &waiting.shape),
        Some(&None)
    );
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    trees.set_remote("m".into(), remote(file("d", "m", 1)));
    trees.forget_lost(
        gone,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert!(trees.waiting().is_empty());
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    let sent =
        build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    assert!(
        sent.contains(&mv("m", ROOT, "x")) && sent.contains(&delete("d")),
        "{sent:?}"
    );
}
