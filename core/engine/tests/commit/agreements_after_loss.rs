// SPDX-License-Identifier: Apache-2.0

#[test]
fn an_edit_the_server_took_first_from_another_device_is_sent_again_as_an_edit_after_a_loss() {
    let mut trees = trees(&[]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "n", 1);
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 101));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
    let taken = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        ..remote(file(ROOT, "n", 2))
    };
    trees.set_remote("n".into(), taken);
    let results = vec![outcome(op_result::Code::Applied)];
    commits.received(result(3, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    trees.set_remote("n".into(), remote_at(file(ROOT, "n", 7), 1));
    trees.forget_lost(
        trashed,
        edited,
        synced,
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
}

/// GUN-453. n's move to b is in flight when L and R agree on c at seq 3 and the other device moves
/// n back to a at seq 4. The server then loses an unrelated change at seq 5, and a bootstrap
/// replaces R. The agreement at seq 3 is from before the loss, so it stays: S takes c and the disk
/// follows the other device's move back to a.
#[test]
fn a_late_acknowledgement_after_a_rollback_keeps_an_agreement_the_server_kept() {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 2));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 3));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "a", 1), 4));
    trees.set_remote("u".into(), remote_at(file(ROOT, "u", 3), 5));
    trees.settle();
    trees.remove_remote("u");
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        4,
        commits.accepted(),
    );
    trees.settle();
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 1)));
    let effects = effects_for(&mut trees, &mut ids, "n");
    assert!(matches!(&effects[..], [Effect::Move { .. }]), "{effects:?}");
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "a", 1)));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

/// GUN-453. n's create at b is applied at seq 8, L and R agree on c at seq 10, and the user moves
/// n back to b. The server then loses commits after seq 11, none of them n's, so the bootstrap's R
/// still holds n at c from seq 10. That agreement survived, so S takes c and the user's move back
/// to b is sent rather than undone.
#[test]
fn a_late_create_acknowledgement_after_a_rollback_keeps_an_agreement_r_still_holds() {
    let mut trees = trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 8));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote(
        "n".into(),
        RemoteNode {
            content_rev: 10,
            edit_rev: 10,
            ..remote_at(file(ROOT, "c", 2), 10)
        },
    );
    trees.set_local("n".into(), local(file(ROOT, "b", 1), 1));
    trees.settle();
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        0,
        commits.accepted(),
    );
    trees.settle();
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 11));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
}

/// GUN-457. n's create at b is applied at seq 8, and L and R agree on c at seq 10. The server then
/// loses every commit after seq 7, so the bootstrap's R lacks n, and the create sent again is
/// applied at seq 9. The record that held the agreement is gone, so S takes b from the result and
/// the user's move to c is sent rather than undone.
#[test]
fn a_late_create_acknowledgement_after_a_rollback_drops_the_agreement_the_server_lost() {
    let mut trees = trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 8));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote(
        "n".into(),
        RemoteNode {
            content_rev: 10,
            edit_rev: 10,
            ..remote_at(file(ROOT, "c", 2), 10)
        },
    );
    trees.settle();
    trees.remove_remote("n");
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        7,
        commits.accepted(),
    );
    trees.settle();
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 9));
    commits
        .received(result(9, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 9));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "b", 1)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "c", 1)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "c")]);
}

/// GUN-457. As above, L and R agree on c at seq 10, and the server loses every commit after seq 7.
/// Its log then reaches seq 12 again with another device's edit of n at b. A revision above the
/// agreed one does not show that the record holding the agreement survived, so the agreement is
/// dropped: the late acknowledgement leaves n at b, and the user's move to c is sent.
#[test]
fn a_reused_revision_does_not_keep_an_agreement_the_server_lost() {
    let mut trees = trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 8));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote(
        "n".into(),
        RemoteNode {
            content_rev: 10,
            edit_rev: 10,
            ..remote_at(file(ROOT, "c", 2), 10)
        },
    );
    trees.settle();
    trees.remove_remote("n");
    trees.set_remote(
        "n".into(),
        RemoteNode {
            content_rev: 12,
            edit_rev: 12,
            ..remote_at(file(ROOT, "b", 2), 12)
        },
    );
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        7,
        commits.accepted(),
    );
    trees.settle();
    commits
        .received(result(9, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 12));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let next = (commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 12)
        .cloned())
    .unwrap();
    assert_eq!(ops(&next), [mv("n", ROOT, "c")]);
}

/// GUN-457: after a loss, an agreement on c at revision 10 is kept only when the server provably
/// holds its record: its revision is at or below `held_through`, or the new R holds the node live at
/// c with that very revision. A later revision at c, another name, the trash or no record drops it.
#[test]
fn an_agreement_survives_a_loss_only_on_a_kept_seq_or_its_exact_record() {
    for (held_through, rev, name, trashed, kept) in [
        (10, 12, "z", false, true),
        (9, 10, "c", false, true),
        (9, 10, "z", false, false),
        (9, 12, "c", false, false),
        (9, 10, "c", true, false),
        (9, 0, "missing", false, false),
    ] {
        let mut trees = trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 8));
        trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
        trees.set_remote("n".into(), remote_at(file(ROOT, "c", 2), 10));
        assert_eq!(trees.position_after("n", 8).unwrap().rev, 10);
        trees.set_local("n".into(), local(file(ROOT, "d", 1), 1));
        trees.remove_remote("n");
        if rev != 0 {
            trees.set_remote(
                "n".into(),
                RemoteNode {
                    trashed,
                    trash_group: if trashed { "g:0".into() } else { String::new() },
                    ..remote_at(file(ROOT, name, 2), rev)
                },
            );
        }
        trees.forget_lost(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Default::default(),
            held_through,
            commits.accepted(),
        );
        trees.settle();
        assert_eq!(
            trees.position_after("n", 8).map(|agreed| agreed.rev),
            kept.then_some(10),
            "held_through={held_through} rev={rev} name={name} trashed={trashed}"
        );
    }
}

#[test]
fn a_resend_lost_in_a_second_loss_is_not_sent_again_as_this_devices_edit() {
    let (mut commits, mut trees, mut ids) = downloaded_at_5();
    list_after_a_loss(
        &mut trees,
        &commits,
        version(remote(file(ROOT, "a", 1)), "v1"),
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [resend("a", 2)]);
    let resent = RemoteNode {
        content_rev: 6,
        ..version(remote(file(ROOT, "a", 2)), "v6")
    };
    trees.set_remote("a".into(), resent);
    let results = vec![outcome(op_result::Code::Applied)];
    commits.received(result(6, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 6));
    trees.settle();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    trees.set_listed_remote("a".into(), version(remote(file(ROOT, "a", 1)), "v1"));
    trees.forget_lost(
        trashed,
        edited,
        synced,
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(!ops(&commit).contains(&update("a", 2)));
}
