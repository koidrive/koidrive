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
        .build(&mut trees, &mut ids, NOW, HISTORY, 12)
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

fn renamed_elsewhere_then_lost() -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[synced("f", file(ROOT, "a", 1))]);
    trees.set_remote("f".into(), remote_at(file(ROOT, "b", 1), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.settle();
    (trees, Commits::default(), IdGen::new(1))
}

fn created_at(commit: &proto::Commit) -> Vec<(String, u8)> {
    (ops(commit).into_iter())
        .filter_map(|op| match op {
            op::Op::CreateFile(create) => {
                let digest = create.content.map(|content| content.manifest_digest[0]);
                Some((create.name, digest.unwrap_or_default()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_resend_whose_lost_folder_the_user_deleted_is_created_where_l_holds_it() {
    let mut trees = trees(&[
        synced("p", dir(ROOT, "p")),
        synced("f", file(ROOT, "a", 1)),
    ]);
    trees.set_remote("f".into(), remote_at(file("p", "b", 1), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.remove_remote("p");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.remove_local("p");
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("a".to_owned(), 1)]);
}

#[test]
fn a_resend_whose_lost_folder_is_excluded_is_created_where_l_holds_it() {
    let mut trees = trees(&[
        synced("p", dir(ROOT, "p")),
        synced("f", file(ROOT, "a", 1)),
    ]);
    trees.set_remote("f".into(), remote_at(file("p", "b", 1), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.remove_remote("p");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"/p/\n")));
    trees.settle();
    assert!(trees.excluded("p"));
    assert!(!trees.excluded("f"));
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).expect("the non-excluded file can upload");
    assert_eq!(created_at(&commit), [("a".to_owned(), 1)]);
}

#[test]
fn a_resend_whose_lost_folder_the_user_moved_into_it_is_created_where_l_holds_it() {
    let mut trees = trees(&[synced("p", dir(ROOT, "p")), synced("q", dir(ROOT, "q"))]);
    trees.set_remote("p".into(), remote_at(dir("q", "p"), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("p");
    trees.remove_remote("q");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.set_local("q".into(), local(dir("p", "q"), 2));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let created: Vec<(String, String)> = (ops(&commit).into_iter())
        .filter_map(|op| match op {
            op::Op::CreateDir(create) => Some((create.node_id, create.parent_id)),
            _ => None,
        })
        .collect();
    assert_eq!(created, [("p".into(), ROOT.into()), ("q".into(), "p".into())]);
}

#[test]
fn a_resent_folder_the_server_merges_into_another_takes_the_position_the_server_held() {
    let mut trees = trees(&[synced("d", dir(ROOT, "x"))]);
    trees.set_remote("d".into(), remote_at(dir(ROOT, "y"), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("d");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateDir(c)] if c.name == "y"));
    trees.set_remote("m".into(), remote(dir(ROOT, "y")));
    let merged = OpResult {
        node_id: "m".into(),
        ..outcome(op_result::Code::MergedInto)
    };
    acknowledge(&mut commits, &mut trees, vec![merged]);
    let next = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert_eq!(next, None);
    effects_for(&mut trees, &mut ids, "m");
    assert_eq!(trees.local().live("m").map(|node| node.name.as_str()), Some("y"));
}

#[test]
fn a_file_in_a_resent_folder_the_server_merges_keeps_the_name_l_holds() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "x")),
        synced("f", file("d", "a", 1)),
    ]);
    trees.set_remote("d".into(), remote_at(dir(ROOT, "y"), 2));
    trees.set_remote("f".into(), remote_at(file("d", "b", 1), 3));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.remove_remote("d");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.set_remote("m".into(), remote(dir(ROOT, "y")));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit).len(), 2);
    assert_eq!(created_at(&commit), [("b".to_owned(), 1)]);
    assert!(trees.state().lost_positions.contains_key("f"));
    trees.set_remote_made_here("f".into(), remote(file("m", "b", 1)));
    let merged = OpResult { node_id: "m".into(), ..outcome(op_result::Code::MergedInto) };
    acknowledge(&mut commits, &mut trees, vec![merged, outcome(op_result::Code::Applied)]);
    let next = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert_eq!(next, None, "rekeying the parent is not a user rename of its child");
    effects_for(&mut trees, &mut ids, "f");
    assert_eq!(trees.local().live("f").map(|node| node.name.as_str()), Some("b"));
}

fn merged_folder_in_split_commits(local_parent: &str, saved_parent: &str, expected_parent: &str) {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "x")),
        synced("f", file(local_parent, "a", 1)),
        synced("q", dir(ROOT, "q")),
    ]);
    trees.set_remote("d".into(), remote_at(dir(ROOT, "y"), 2));
    trees.set_remote("f".into(), remote_at(file(saved_parent, "b", 1), 3));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.remove_remote("d");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.set_remote("m".into(), remote(dir(ROOT, "y")));
    trees.settle();
    let state = CommitsState { cap: Some(1), ..Default::default() };
    let (mut commits, mut ids) = (Commits::from_state(state).unwrap(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    if local_parent == "d" {
        assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(create)] if create.node_id == "f" && create.parent_id == "q" && create.name == "b"));
        trees.set_remote_made_here("f".into(), remote(file("q", "b", 1)));
        acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied)]);
        let parent = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert!(matches!(&ops(&parent)[..], [op::Op::CreateDir(create)] if create.node_id == "d"));
        let merged = OpResult { node_id: "m".into(), ..outcome(op_result::Code::MergedInto) };
        commits.received(result(2, vec![merged])).unwrap();
        assert!(commits.acknowledge(&mut trees, 2));
        trees.settle();
        let next = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
        assert_eq!(next, None, "a structural parent remap must not undo the child's acknowledged destination");
        effects_for(&mut trees, &mut ids, "f");
        assert_eq!(trees.local().live("f").map(|node| (node.parent.as_str(), node.name.as_str())), Some(("q", "b")));
        return;
    }
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateDir(create)] if create.node_id == "d"));
    let merged = OpResult { node_id: "m".into(), ..outcome(op_result::Code::MergedInto) };
    acknowledge(&mut commits, &mut trees, vec![merged]);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let planned = ops(&commit);
    assert!(matches!(&planned[..], [op::Op::CreateFile(create)] if create.node_id == "f" && create.parent_id == expected_parent && create.name == "b"), "the saved destination must survive the parent ID remap: {planned:?}");
}

#[test]
fn a_file_sent_to_another_folder_keeps_that_position_when_the_server_merges_its_local_folder() {
    merged_folder_in_split_commits("d", "q", "q");
}

#[test]
fn a_file_whose_lost_folder_the_server_merges_is_created_in_the_folder_it_merges_into() {
    merged_folder_in_split_commits(ROOT, "d", "m");
}

#[test]
fn a_resend_the_server_creates_under_another_name_is_not_sent_again_as_an_edit() {
    let mut trees = trees(&[synced("f", file(ROOT, "a", 1))]);
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.set_remote("d".into(), remote(dir(ROOT, "a")));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("a".to_owned(), 1)]);
    trees.set_remote_made_here("f".into(), remote(file(ROOT, "a (conflict)", 1)));
    let renamed = OpResult {
        name: "a (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    effects_for(&mut trees, &mut ids, "f");
    assert_eq!(trees.local().live("f").map(|node| node.name.as_str()), Some("a (conflict)"));
    let next = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert_eq!(next, None);
}

#[test]
fn a_resend_into_a_lost_folder_l_still_holds_is_created_in_it() {
    let mut trees = trees(&[
        synced("p", dir(ROOT, "p")),
        synced("f", file(ROOT, "a", 1)),
    ]);
    trees.set_remote("f".into(), remote_at(file("p", "b", 1), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.remove_remote("p");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 1)]);
    assert!(ops(&commit).iter().any(|op| matches!(op, op::Op::CreateDir(create) if create.node_id == "p")));
}

#[test]
fn a_resend_edited_before_it_is_sent_is_created_where_the_server_held_it() {
    let (mut trees, mut commits, mut ids) = renamed_elsewhere_then_lost();
    trees.set_local("f".into(), local(file(ROOT, "a", 2), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 2)]);
}

#[test]
fn a_resend_edited_while_its_create_is_in_flight_takes_the_position_the_server_held() {
    let (mut trees, mut commits, mut ids) = renamed_elsewhere_then_lost();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 1)]);
    trees.set_local("f".into(), local(file(ROOT, "a", 2), 1));
    trees.set_remote_made_here("f".into(), remote_at(file(ROOT, "b", 1), 1));
    acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied)]);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("f", 2)]);
    let updated = RemoteNode {
        content_rev: 2,
        ..remote_at(file(ROOT, "b", 2), 1)
    };
    trees.set_remote_made_here("f".into(), updated);
    commits.received(result(2, vec![outcome(op_result::Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    effects_for(&mut trees, &mut ids, "f");
    assert_eq!(trees.local().live("f").map(|node| node.name.as_str()), Some("b"));
}

#[test]
fn a_resend_whose_record_r_drops_before_its_acknowledgement_is_not_created_at_the_old_name() {
    let (mut trees, mut commits, mut ids) = renamed_elsewhere_then_lost();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote_made_here("f".into(), remote_at(file(ROOT, "b", 1), 1));
    trees.remove_remote("f");
    acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied)]);
    let sent = build(&mut commits, &mut trees, &mut ids);
    assert!(sent.is_none_or(|commit| created_at(&commit).is_empty()));
    trees.set_remote_made_here("f".into(), remote_at(file(ROOT, "b", 1), 1));
    trees.settle();
    assert_eq!(trees.class("f"), Class::Remote);
}

#[test]
fn a_file_another_device_renamed_and_edited_before_a_loss_is_created_at_its_new_name() {
    let renamed = RemoteNode {
        content_rev: 3,
        version_id: "v2".into(),
        ..remote_at(file(ROOT, "b", 2), 2)
    };
    let held = Some(file(ROOT, "a", 1));
    let mut trees = trees(&[("f", held.clone(), held, Some(renamed))]);
    let untaken = trees.untaken_changes(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), Vec::new(), Vec::new(), untaken, Default::default(), 0, 0);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 1)]);
}

#[test]
fn a_second_loss_before_l_takes_a_resends_position_creates_it_there_again() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "x")),
        synced("f", file("d", "a", 1)),
        synced("q", dir(ROOT, "q")),
    ]);
    trees.set_remote("d".into(), remote_at(dir(ROOT, "y"), 2));
    trees.set_remote("f".into(), remote_at(file("q", "b", 1), 3));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.remove_remote("d");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.set_remote("m".into(), remote(dir(ROOT, "y")));
    trees.settle();
    let state = CommitsState { cap: Some(1), ..Default::default() };
    let (mut commits, mut ids) = (Commits::from_state(state).unwrap(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(create)] if create.node_id == "f" && create.parent_id == "q" && create.name == "b"));
    trees.set_remote_made_here("f".into(), remote_at(file("q", "b", 1), 4));
    commits.received(result(4, vec![outcome(op_result::Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle();
    assert_eq!(trees.waiting().get("f").unwrap().origin, Origin::Commit(commit.device_seq));
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), untaken, Default::default(), commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle();
    assert!(!trees.waiting().contains_key("f"));
    let next = build(&mut commits, &mut trees, &mut ids).unwrap();
    let planned = ops(&next);
    let position = planned.iter().find_map(|op| match op {
        op::Op::CreateFile(create) if create.node_id == "f" => Some((create.parent_id.as_str(), create.name.as_str())),
        _ => None,
    });
    assert_eq!(position, Some(("q", "b")), "the repeated loss must keep the known remote destination: {planned:?}");
}

#[test]
fn a_resend_the_server_renames_to_the_name_l_holds_leaves_no_lost_position() {
    let conflict = "b (conflict from Mini 2026-11-14)";
    let mut trees = trees(&[synced("f", file(ROOT, conflict, 1))]);
    trees.set_remote("f".into(), remote_at(file(ROOT, "b", 1), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.set_remote("d".into(), remote(dir(ROOT, "b")));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(create)] if create.name == "b"));
    trees.set_remote_made_here("f".into(), remote(file(ROOT, conflict, 1)));
    let renamed = OpResult {
        name: conflict.into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    assert_eq!(trees.class("f"), Class::InSync);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(trees.state().lost_positions.is_empty());
}

#[test]
fn a_resend_the_server_merges_into_a_folder_and_renames_to_the_name_l_holds_leaves_no_lost_position() {
    let conflict = "b (conflict from Mini 2026-11-14)";
    let mut trees = trees(&[synced("d", dir(ROOT, "x")), synced("f", file("d", conflict, 1))]);
    trees.set_remote("d".into(), remote_at(dir(ROOT, "y"), 2));
    trees.set_remote("f".into(), remote_at(file("d", "b", 1), 3));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.remove_remote("d");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), 0, 0);
    trees.set_remote("m".into(), remote(dir(ROOT, "y")));
    trees.set_remote("g".into(), remote(dir("m", "b")));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateDir(parent), op::Op::CreateFile(child)] if parent.node_id == "d" && child.node_id == "f" && child.parent_id == "d" && child.name == "b"));
    trees.set_remote_made_here("f".into(), remote(file("m", conflict, 1)));
    let merged = OpResult {
        node_id: "m".into(),
        ..outcome(op_result::Code::MergedInto)
    };
    let renamed = OpResult {
        name: conflict.into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![merged, renamed]);
    assert_eq!(trees.local().live("f"), Some(&file("m", conflict, 1)));
    assert_eq!(trees.class("f"), Class::InSync);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(trees.state().lost_positions.is_empty());
}

#[test]
fn a_resend_another_device_renames_before_its_acknowledgement_takes_that_name() {
    let (mut trees, mut commits, mut ids) = renamed_elsewhere_then_lost();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 1)]);
    trees.set_remote_made_here("f".into(), remote_at(file(ROOT, "b", 1), 3));
    trees.set_remote("f".into(), remote_at(file(ROOT, "c", 1), 4));
    acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied)]);
    assert_eq!(trees.class("f"), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "f");
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "c", 1)));
    assert!(trees.state().lost_positions.is_empty());
}

fn acknowledged_resend_at_its_lost_position(peer_name: Option<&str>) -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[synced("f", file(ROOT, "a", 1))]);
    let old = RemoteNode { version_id: "old".into(), ..remote_at(file(ROOT, "b", 1), 2) };
    trees.set_remote("f".into(), old);
    let synced = trees.taken_from_others(|_| false);
    let records = trees.remote_records();
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), records, 0, 0);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = commits.build(&mut trees, &mut ids, NOW, HISTORY, 0).cloned().unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(create)] if create.name == "b"));
    let own = RemoteNode { version_id: "resent".into(), ..remote(file(ROOT, "b", 1)) };
    trees.set_remote_made_here("f".into(), own);
    if let Some(name) = peer_name {
        let peer = RemoteNode { version_id: "resent".into(), ..remote_at(file(ROOT, name, 1), 2) };
        trees.set_remote("f".into(), peer);
    }
    commits.received(result(1, vec![outcome(op_result::Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, if peer_name.is_some() { 2 } else { 1 }));
    trees.settle();
    assert_eq!(trees.class("f"), Class::Remote);
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "a", 1)));
    (trees, commits, ids)
}

#[test]
fn a_second_loss_after_another_device_renamed_a_resend_creates_it_at_that_name() {
    let (mut trees, mut commits, mut ids) = acknowledged_resend_at_its_lost_position(Some("c"));
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    assert!(commits.lost_here(1));
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    assert!(synced.is_empty());
    let moved = trees.moved_here();
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records();
    assert_eq!(records.get("f").map(|node| node.shape.name.as_str()), Some("c"));
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, moved, untaken, records, commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle();
    let commit = commits.build(&mut trees, &mut ids, NOW, HISTORY, 0).cloned().unwrap();
    let planned = ops(&commit);
    assert!(matches!(&planned[..], [op::Op::CreateFile(create)] if create.node_id == "f" && create.name == "c"), "a later loss must not revive the older saved destination: {planned:?}; saved positions: {:?}", trees.state().lost_positions);
}

#[test]
fn a_rename_another_device_makes_during_a_second_resend_is_not_undone() {
    let (mut trees, mut commits, mut ids) = acknowledged_resend_at_its_lost_position(None);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let moved = trees.moved_here();
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records();
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, moved, untaken, records, commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle();
    let commit = commits.build(&mut trees, &mut ids, NOW, HISTORY, 0).cloned().unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(create)] if create.name == "b"));
    assert!(trees.state().lost_positions.contains_key("f"));
    assert!(!trees.state().resends.contains("f"));
    let own = RemoteNode { version_id: "resent-again".into(), ..remote(file(ROOT, "b", 1)) };
    trees.set_remote_made_here("f".into(), own);
    let peer = RemoteNode { version_id: "resent-again".into(), ..remote_at(file(ROOT, "c", 1), 2) };
    trees.set_remote("f".into(), peer);
    commits.received(result(1, vec![outcome(op_result::Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    let class = trees.class("f");
    let next = commits.build(&mut trees, &mut ids, NOW, HISTORY, 2).cloned().map(|commit| ops(&commit));
    assert_eq!(next, None, "a peer rename during repeated recovery must remain remote, not be undone: {class:?}");
    effects_for(&mut trees, &mut ids, "f");
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "c", 1)));
    assert!(trees.state().lost_positions.is_empty());
}
