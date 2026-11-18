// SPDX-License-Identifier: Apache-2.0

#[test]
fn an_edit_the_server_took_first_from_another_device_is_sent_again_as_an_edit_after_a_loss() {
    let mut trees = trees(&[]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "n", 1);
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 101));
    trees.settle(commits.aliases());
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
    trees.settle(commits.aliases());
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
        &Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle(commits.aliases());
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
    trees.settle(commits.aliases());
    trees.remove_remote("u");
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        &Default::default(),
        4,
        commits.accepted(),
    );
    trees.settle(commits.aliases());
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle(commits.aliases());
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
    trees.settle(commits.aliases());
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        &Default::default(),
        0,
        commits.accepted(),
    );
    trees.settle(commits.aliases());
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 11));
    trees.settle(commits.aliases());
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
    trees.settle(commits.aliases());
    trees.remove_remote("n");
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        &Default::default(),
        7,
        commits.accepted(),
    );
    trees.settle(commits.aliases());
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 9));
    commits
        .received(result(9, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 9));
    trees.settle(commits.aliases());
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
    trees.settle(commits.aliases());
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
        &Default::default(),
        7,
        commits.accepted(),
    );
    trees.settle(commits.aliases());
    commits
        .received(result(9, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 12));
    trees.settle(commits.aliases());
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
            &Default::default(),
            held_through,
            commits.accepted(),
        );
        trees.settle(commits.aliases());
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
    assert_eq!(ops(&commit), [resend("a", 2, "v2")]);
    let resent = RemoteNode {
        content_rev: 6,
        ..version(remote(file(ROOT, "a", 2)), "v6")
    };
    trees.set_remote("a".into(), resent);
    let results = vec![outcome(op_result::Code::Applied)];
    commits.received(result(6, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 6));
    trees.settle(commits.aliases());
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
        &Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle(commits.aliases());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(!ops(&commit).contains(&update("a", 2)));
}

fn renamed_elsewhere_then_lost() -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[synced("f", file(ROOT, "a", 1))]);
    trees.set_remote("f".into(), remote_at(file(ROOT, "b", 1), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.settle(&Default::default());
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
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.remove_local("p");
    trees.settle(&Default::default());
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
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"/p/\n")));
    trees.settle(&Default::default());
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
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.set_local("q".into(), local(dir("p", "q"), 2));
    trees.settle(&Default::default());
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

fn a_resend_below_a_cycle_of_lost_folders_keeps_the_position_another_device_gave_it(file_id: &'static str) {
    let mut trees = trees(&[
        synced("p", dir(ROOT, "p")),
        synced("q", dir(ROOT, "q")),
        synced("r", dir(ROOT, "r")),
        synced(file_id, file(ROOT, "a", 1)),
    ]);
    trees.set_remote("p".into(), remote_at(dir("q", "p"), 2));
    trees.set_remote("q".into(), remote_at(dir("r", "q"), 3));
    trees.set_remote(file_id.into(), remote_at(file("p", "b", 1), 4));
    let synced = trees.taken_from_others(|_| false);
    let records = trees.remote_records(|_| false);
    for id in [file_id, "p", "q", "r"] {
        trees.remove_remote(id);
    }
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), records, &Default::default(), 0, 0);
    trees.set_local_changed("r".into(), local(dir("p", "r"), 3));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let created = (ops(&commit).into_iter()).find_map(|op| match op {
        op::Op::CreateFile(create) if create.node_id == file_id => Some((create.parent_id, create.name)),
        _ => None,
    });
    assert_eq!(created, Some(("p".into(), "b".into())), "{:?}", ops(&commit));
}

#[test]
fn a_resend_ordered_before_a_cycle_of_lost_folders_keeps_the_position_another_device_gave_it() {
    a_resend_below_a_cycle_of_lost_folders_keeps_the_position_another_device_gave_it("f");
}

#[test]
fn a_resend_ordered_after_a_cycle_of_lost_folders_keeps_the_position_another_device_gave_it() {
    a_resend_below_a_cycle_of_lost_folders_keeps_the_position_another_device_gave_it("z");
}

#[test]
fn a_resend_in_a_lost_folder_the_user_moved_into_it_keeps_the_position_another_device_gave_it() {
    let mut trees = trees(&[
        synced("p", dir(ROOT, "p")),
        synced("q", dir(ROOT, "q")),
        synced("f", file(ROOT, "a", 1)),
    ]);
    trees.set_remote("p".into(), remote_at(dir("q", "p"), 2));
    trees.set_remote("f".into(), remote_at(file("p", "b", 1), 3));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.remove_remote("p");
    trees.remove_remote("q");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.set_local("q".into(), local(dir("p", "q"), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let created: std::collections::BTreeSet<(String, String, String)> = (ops(&commit).into_iter())
        .filter_map(|op| match op {
            op::Op::CreateDir(create) => Some((create.node_id, create.parent_id, create.name)),
            op::Op::CreateFile(create) => Some((create.node_id, create.parent_id, create.name)),
            _ => None,
        })
        .collect();
    let expected = [("f", "p", "b"), ("p", ROOT, "p"), ("q", "p", "q")]
        .map(|(id, parent, name)| (id.to_owned(), parent.to_owned(), name.to_owned()));
    assert_eq!(created, std::collections::BTreeSet::from(expected));
}

#[test]
fn a_resent_folder_the_server_merges_into_another_takes_the_position_the_server_held() {
    let mut trees = trees(&[synced("d", dir(ROOT, "x"))]);
    trees.set_remote("d".into(), remote_at(dir(ROOT, "y"), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("d");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.settle(&Default::default());
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
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.set_remote("m".into(), remote(dir(ROOT, "y")));
    trees.settle(&Default::default());
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
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.set_remote("m".into(), remote(dir(ROOT, "y")));
    trees.settle(&Default::default());
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
        trees.settle(commits.aliases());
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
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.set_remote("d".into(), remote(dir(ROOT, "a")));
    trees.settle(&Default::default());
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
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 1)]);
    assert!(ops(&commit).iter().any(|op| matches!(op, op::Op::CreateDir(create) if create.node_id == "p")));
}

#[test]
fn a_folder_resent_for_a_file_the_user_renamed_in_it_is_created_with_the_rename() {
    let mut trees = held_elsewhere_in_a_lost_folder();
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateDir(create), moved] if create.node_id == "p" && *moved == mv("f", "p", "b")));
}

#[test]
fn a_folder_resent_for_a_file_the_user_moved_into_a_new_folder_in_it_is_created_with_both() {
    let mut trees = held_elsewhere_in_a_lost_folder();
    trees.set_local_changed("q".into(), local(dir("p", "q"), 3));
    trees.set_local_changed("f".into(), local(file("q", "a", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateDir(p), op::Op::CreateDir(q), moved] if p.node_id == "p" && q.node_id == "q" && *moved == mv("f", "q", "a")));
}

#[test]
fn a_folder_resent_for_a_file_the_user_renamed_in_it_is_created_when_another_device_deleted_the_file() {
    let mut trees = trees(&[
        ("p", Some(dir(ROOT, "p")), Some(dir(ROOT, "p")), None),
        ("f", Some(file("p", "a", 1)), Some(file("p", "a", 1)), Some(trashed(file(ROOT, "c", 1)))),
    ]);
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && *moved == mv("f", "p", "b")), "{commit:?}");
}

#[test]
fn a_folder_resent_for_a_file_whose_s_update_waits_keeps_the_waiting_content_and_origin() {
    let mut trees = trees(&[
        ("p", Some(dir(ROOT, "p")), Some(dir(ROOT, "p")), Some(remote(dir(ROOT, "p")))),
        ("h", Some(file("p", "x", 9)), Some(file("p", "x", 9)), Some(remote(file("p", "y", 9)))),
        ("f", Some(file("p", "x", 1)), Some(file("p", "a", 0)), Some(remote(file("p", "a", 0)))),
        synced("w", file(ROOT, "c", 5)),
    ]);
    trees.set_local("h".into(), LocalNode { parked: true, ..local(file("p", "x", 9), 2) });
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", "p", "x"), update("f", 1)]);
    trees.set_remote("f".into(), remote(file("p", "x", 1)));
    acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied); 2]);
    let waiting = |shape| Some(Waiting { shape: Some(shape), origin: Origin::Commit(1) });
    assert_eq!(trees.waiting().get("f").cloned(), waiting(file("p", "x", 1)));
    trees.remove_local("h");
    trees.set_synced("h".into(), None);
    trees.remove_remote("h");
    trees.remove_remote("w");
    trees.set_remote("f".into(), remote(file(ROOT, "c", 1)));
    trees.remove_remote("p");
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 3));
    trees.settle(&Default::default());
    let commit = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && *moved == mv("f", "p", "b")), "{commit:?}");
    assert_eq!(trees.synced().live("f").map(|shape| &shape.content), Some(&file("p", "x", 1).content));
}

#[test]
fn a_folder_resent_for_a_file_the_user_renamed_in_it_is_created_when_another_device_deleted_the_file_in_a_deleted_folder() {
    let mut trees = trees(&[
        ("p", Some(dir(ROOT, "p")), Some(dir(ROOT, "p")), None),
        ("t", None, None, Some(trashed(dir(ROOT, "t")))),
        ("f", Some(file("p", "a", 1)), Some(file("p", "a", 1)), Some(trashed(file("t", "c", 1)))),
    ]);
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && *moved == mv("f", "p", "b")), "{commit:?}");
}

#[test]
fn a_folder_resent_for_a_file_the_user_renamed_in_it_is_created_when_another_device_deleted_the_file_and_its_folder() {
    let mut trees = trees(&[
        ("p", Some(dir(ROOT, "p")), Some(dir(ROOT, "p")), None),
        ("g", None, None, Some(trashed(dir(ROOT, "g")))),
        ("f", Some(file("p", "a", 1)), Some(file("p", "a", 1)), Some(trashed(file("g", "c", 1)))),
    ]);
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && *moved == mv("f", "p", "b")), "{commit:?}");
}

#[test]
fn a_folder_resent_for_a_folder_the_user_renamed_in_it_is_created_when_another_device_moved_it_into_its_own_subfolder() {
    let mut trees = trees(&[
        ("p", Some(dir(ROOT, "p")), Some(dir(ROOT, "p")), None),
        ("d", Some(dir("p", "d")), Some(dir("p", "d")), Some(remote(dir("e", "d")))),
        ("e", Some(dir("d", "e")), Some(dir("d", "e")), Some(remote(dir(ROOT, "e")))),
    ]);
    trees.set_local_changed("d".into(), local(dir("p", "d2"), 2));
    trees.set_local_changed("e".into(), local(dir("d", "e2"), 3));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), first, second]) if create.node_id == "p" && *first == mv("d", "p", "d2") && *second == mv("e", "d", "e2")), "{commit:?}");
}

fn resent_for_a_file_trashed_in_a_deleted_folder() -> Trees {
    trees(&[
        ("p", Some(dir(ROOT, "p")), Some(dir(ROOT, "p")), None),
        ("t", None, None, Some(trashed(dir(ROOT, "t")))),
        ("f", Some(file("p", "a", 1)), Some(file("p", "a", 1)), Some(trashed(file("t", "c", 1)))),
    ])
}

fn renamed_in_a_resent_folder() -> (Trees, Commits, IdGen) {
    let mut trees = resent_for_a_file_trashed_in_a_deleted_folder();
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    (trees, Commits::default(), IdGen::new(1))
}

#[test]
fn a_file_the_user_created_in_a_resent_folder_is_created_after_the_folder() {
    let mut trees = held_elsewhere_in_a_lost_folder();
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.set_local_changed("n".into(), local(file("p", "n", 3), 3));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert!(matches!(&commit[0], op::Op::CreateDir(create) if create.node_id == "p"), "{commit:?}");
    assert!(commit.iter().any(|op| matches!(op, op::Op::CreateFile(create) if create.node_id == "n" && create.parent_id == "p")), "{commit:?}");
    assert!(commit.contains(&mv("f", "p", "b")), "{commit:?}");
}

#[test]
fn a_resent_folder_is_not_removed_before_the_server_acknowledges_its_create() {
    let (mut trees, mut commits, mut ids) = renamed_in_a_resent_folder();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(effects_for(&mut trees, &mut ids, "p"), []);
    assert_eq!(effects_for(&mut trees, &mut ids, "f"), []);
    assert_eq!(trees.local().live("p"), Some(&dir(ROOT, "p")));
    assert_eq!(trees.local().live("f"), Some(&file("p", "b", 1)));
}

#[test]
fn a_resent_folder_settles_once_the_server_lists_its_create() {
    let (mut trees, mut commits, mut ids) = renamed_in_a_resent_folder();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote_made_here("p".into(), remote(dir(ROOT, "p")));
    trees.set_remote_made_here("f".into(), remote(file("p", "b", 1)));
    acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied); 2]);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(effects_for(&mut trees, &mut ids, "p"), []);
    assert_eq!((trees.class("p"), trees.class("f")), (Class::InSync, Class::InSync));
    assert!(trees.state().unsent_creates.is_empty());
}

#[test]
fn a_resent_folder_acknowledged_before_its_record_arrives_is_not_created_again() {
    let (mut trees, mut commits, mut ids) = renamed_in_a_resent_folder();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied); 2]);
    assert!(trees.state().unsent_creates.is_empty());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_resent_folder_the_user_deleted_after_its_create_was_refused_is_not_created() {
    let (mut trees, mut commits, mut ids) = renamed_in_a_resent_folder();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    commits.received(result(1, vec![outcome(op_result::Code::Rejected); 2])).unwrap();
    commits.acknowledge(&mut trees, 1);
    trees.remove_local("f");
    trees.remove_local("p");
    trees.settle(commits.aliases());
    let next = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(!next.iter().flatten().any(|op| matches!(op, op::Op::CreateDir(_) | op::Op::CreateFile(_) | op::Op::Move(_))), "{next:?}");
    assert!(trees.state().unsent_creates.is_empty());
}

#[test]
fn a_resent_folder_merged_into_another_devices_folder_leaves_nothing_to_send() {
    let mut trees = held_elsewhere_in_a_lost_folder();
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("m".into(), remote(dir(ROOT, "p")));
    trees.set_remote_made_here("f".into(), remote(file("m", "b", 1)));
    let merged = OpResult { node_id: "m".into(), ..outcome(op_result::Code::MergedInto) };
    acknowledge(&mut commits, &mut trees, vec![merged, outcome(op_result::Code::Applied)]);
    assert!(trees.state().unsent_creates.is_empty());
    for _ in 0..3 {
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    }
    assert_eq!(trees.local().live("f"), Some(&file("m", "b", 1)));
}

#[test]
fn a_resent_folder_the_server_loses_again_after_applying_its_create_is_created_again() {
    let mut trees = held_elsewhere_in_a_lost_folder();
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    trees.set_remote_made_here("p".into(), remote_at(dir(ROOT, "p"), 1));
    trees.set_remote_made_here("f".into(), remote_at(file("p", "b", 1), 1));
    acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied); first.len()]);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited, moved) = (trees.trashed_here(), trees.edited_here(), trees.moved_here());
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("p");
    trees.set_remote("f".into(), remote(file(ROOT, "c", 1)));
    trees.forget_lost(trashed, edited, synced, moved, Vec::new(), Default::default(), &Default::default(), commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle(commits.aliases());
    let again = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert!(matches!(&again[0], op::Op::CreateDir(create) if create.node_id == "p"), "{again:?}");
    assert!(again.iter().any(|op| matches!(op, op::Op::Move(moved) if moved.node_id == "f" && moved.new_parent_id == "p" && moved.new_name == "b")), "{again:?}");
}

#[test]
fn a_resent_folder_keeps_its_create_across_a_restart() {
    let (mut trees, mut commits, mut ids) = renamed_in_a_resent_folder();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let state = trees.state();
    assert!(state.unsent_creates.contains("p"));
    let mut restarted = resent_for_a_file_trashed_in_a_deleted_folder();
    restarted.set_local("f".into(), local(file("p", "b", 1), 2));
    restarted.restore(state).unwrap();
    restarted.rules_scanned();
    restarted.settle(&Default::default());
    assert!(restarted.state().unsent_creates.contains("p"));
    assert_eq!(restarted.class("p"), Class::Concurrent);
    assert_eq!(effects_for(&mut restarted, &mut ids, "p"), []);
    assert_eq!(effects_for(&mut restarted, &mut ids, "f"), []);
    assert_eq!(restarted.local().live("p"), Some(&dir(ROOT, "p")));
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut restarted, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && *moved == mv("f", "p", "b")), "{commit:?}");
}

#[test]
fn a_resent_folder_another_device_created_again_and_deleted_keeps_s_and_its_resend() {
    let (mut trees, mut commits, mut ids) = renamed_in_a_resent_folder();
    let commit = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert!(matches!(&commit[..], [op::Op::CreateDir(create), _] if create.node_id == "p"), "{commit:?}");
    trees.set_remote("p".into(), trashed(dir(ROOT, "p")));
    trees.set_remote_made_here("f".into(), remote(file("p", "b", 1)));
    let exists = OpResult { reject_reason: "ID_EXISTS".into(), ..outcome(op_result::Code::Rejected) };
    acknowledge(&mut commits, &mut trees, vec![exists, outcome(op_result::Code::Applied)]);
    assert!(commits.problems().is_empty());
    assert!(trees.state().unsent_creates.is_empty());
    assert_eq!(trees.synced().live("p"), Some(&dir(ROOT, "p")));
    assert!(trees.state().resends.contains("p"), "{:?}", trees.state().resends);
}

#[test]
fn a_resent_folder_the_user_renamed_is_created_at_its_new_name() {
    let mut trees = resent_for_a_file_trashed_in_a_deleted_folder();
    trees.set_local_changed("p".into(), local(dir(ROOT, "p2"), 3));
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && create.parent_id == ROOT && create.name == "p2" && *moved == mv("f", "p", "b")), "{commit:?}");
}

#[test]
fn a_resent_folder_the_user_moved_into_another_folder_is_created_there() {
    let mut trees = resent_for_a_file_trashed_in_a_deleted_folder();
    trees.set_remote("q".into(), remote(dir(ROOT, "q")));
    trees.set_local("q".into(), local(dir(ROOT, "q"), 4));
    trees.set_synced("q".into(), Some(dir(ROOT, "q")));
    trees.set_local_changed("p".into(), local(dir("q", "p"), 3));
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && create.parent_id == "q" && create.name == "p" && *moved == mv("f", "p", "b")), "{commit:?}");
}

#[test]
fn a_folder_the_user_created_in_a_resent_folder_is_never_marked_as_resent() {
    let (mut trees, mut commits, mut ids) = renamed_in_a_resent_folder();
    trees.set_local_changed("n".into(), local(dir("p", "n"), 3));
    trees.set_local_changed("g".into(), local(file("n", "g", 4), 4));
    trees.settle(&Default::default());
    let commit = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert!(commit.iter().any(|op| matches!(op, op::Op::CreateDir(create) if create.node_id == "n" && create.parent_id == "p")), "{commit:?}");
    assert_eq!(trees.state().unsent_creates, ["p".into()].into());
}

#[test]
fn a_folder_the_user_created_whose_create_the_server_lost_is_created_again_without_a_mark() {
    let mut trees = trees(&[
        ("p", Some(dir(ROOT, "p")), None, None),
        ("f", Some(file("p", "a", 1)), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert_eq!(first.len(), 2, "{first:?}");
    trees.set_remote_made_here("p".into(), remote_at(dir(ROOT, "p"), 1));
    trees.set_remote_made_here("f".into(), remote_at(file("p", "a", 1), 1));
    commits.received(result(1, vec![outcome(op_result::Code::Applied); 2])).unwrap();
    assert!(commits.acknowledge(&mut trees, 1));
    trees.settle(commits.aliases());
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(commits.aliases());
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited, moved) = (trees.trashed_here(), trees.edited_here(), trees.moved_here());
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records(|seq| commits.lost_here(seq));
    trees.remove_remote("f");
    trees.remove_remote("p");
    trees.forget_lost(trashed, edited, synced, moved, untaken, records, &Default::default(), commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle(commits.aliases());
    assert_eq!(trees.synced().live("p"), None);
    let again = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert!(again.iter().any(|op| matches!(op, op::Op::CreateDir(create) if create.node_id == "p")), "{again:?}");
    assert!(again.iter().any(|op| matches!(op, op::Op::CreateFile(create) if create.node_id == "f" && create.parent_id == "p" && create.name == "b")), "{again:?}");
    assert!(trees.state().unsent_creates.is_empty());
}

#[test]
fn a_folder_the_user_created_that_another_device_deleted_and_purged_is_marked_for_a_change_in_it() {
    let mut trees = trees(&[
        ("p", Some(dir(ROOT, "p")), None, None),
        ("f", Some(file("p", "a", 1)), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote_made_here("p".into(), remote_at(dir(ROOT, "p"), 1));
    trees.set_remote_made_here("f".into(), remote_at(file("p", "a", 1), 1));
    commits.received(result(1, vec![outcome(op_result::Code::Applied); 2])).unwrap();
    assert!(commits.acknowledge(&mut trees, 1));
    trees.settle(commits.aliases());
    trees.set_remote("f".into(), trashed(file("p", "a", 1)));
    trees.remove_remote("p");
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(commits.aliases());
    let commit = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert!(commit.iter().any(|op| matches!(op, op::Op::CreateDir(create) if create.node_id == "p")), "{commit:?}");
    assert_eq!(trees.state().unsent_creates, ["p".into()].into());
}

#[test]
fn a_resent_folder_whose_removal_a_store_from_before_queued_is_created_with_the_rename() {
    let (mut trees, mut commits, mut ids) = renamed_in_a_resent_folder();
    let mut state = trees.state();
    state.waiting.insert("p".into(), Waiting { shape: None, origin: Origin::Other });
    state.resends.insert("p".into());
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle(&Default::default());
    assert_eq!(effects_for(&mut trees, &mut ids, "p"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && *moved == mv("f", "p", "b")), "{commit:?} {:?}", trees.waiting());
    assert_eq!(trees.synced().live("f"), Some(&file("p", "a", 1)));
}

#[test]
fn a_resent_folder_whose_removal_a_loss_queued_is_created_with_the_rename() {
    let (mut trees, mut commits, mut ids) = renamed_in_a_resent_folder();
    trees.forget_lost(Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.settle(&Default::default());
    assert_eq!(trees.waiting().get("p"), Some(&Waiting { shape: None, origin: Origin::Other }));
    let commit = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && *moved == mv("f", "p", "b")), "{commit:?} {:?}", trees.waiting());
}

#[test]
fn a_resent_folder_lost_again_before_its_create_is_acknowledged_is_sent_again() {
    let (mut trees, mut commits, mut ids) = renamed_in_a_resent_folder();
    trees.set_remote("f".into(), trashed(file(ROOT, "c", 1)));
    trees.settle(&Default::default());
    commits.adopt(1);
    let sent = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert!(matches!(&sent[..], [op::Op::CreateDir(create), moved] if create.node_id == "p" && *moved == mv("f", "p", "b")), "{sent:?}");
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let records = trees.remote_records(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let (trashed, edited, moved) = (trees.trashed_here(), trees.edited_here(), trees.moved_here());
    trees.forget_lost(trashed, edited, synced, moved, untaken, records, commits.aliases(), commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle(commits.aliases());
    let next = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(next.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && *moved == mv("f", "p", "b")), "{next:?} {:?}", trees.waiting());
}

#[test]
fn a_resend_merged_into_a_folder_l_holds_keeps_the_rename_in_it() {
    let (mut trees, _, mut ids) = renamed_in_a_resent_folder();
    let mut commits = Commits::from_state(CommitsState { cap: Some(1), ..Default::default() }).unwrap();
    trees.set_synced("m".into(), Some(dir(ROOT, "other")));
    trees.set_local("m".into(), local(dir(ROOT, "other"), 9));
    trees.set_remote("m".into(), remote(dir(ROOT, "other")));
    trees.settle(commits.aliases());
    let first = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert!(matches!(&first[..], [op::Op::CreateDir(create)] if create.node_id == "p"), "{first:?}");
    trees.set_remote("m".into(), remote_at(dir(ROOT, "p"), 6));
    let merged = OpResult { node_id: "m".into(), ..outcome(op_result::Code::MergedInto) };
    acknowledge(&mut commits, &mut trees, vec![merged]);
    let next = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(next.as_deref(), Some([op::Op::Move(moved)]) if moved.node_id == "f" && moved.new_parent_id == "m" && moved.new_name == "b"), "{next:?}");
}

#[test]
fn a_resent_folder_whose_rename_waits_in_s_is_created_where_l_holds_it() {
    let (mut trees, mut commits, mut ids) = renamed_in_a_resent_folder();
    let mut state = trees.state();
    state.waiting.insert("p".into(), Waiting { shape: Some(dir(ROOT, "q")), origin: Origin::Other });
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle(&Default::default());
    let commit = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && create.name == "p" && *moved == mv("f", "p", "b")), "{commit:?} {:?}", trees.waiting());
}

#[test]
fn a_resent_folder_whose_other_child_waits_to_leave_s_is_created_with_the_rename() {
    let mut trees = resent_for_a_file_trashed_in_a_deleted_folder();
    trees.set_local("g".into(), local(file("p", "g", 5), 5));
    trees.set_synced("g".into(), Some(file("p", "g", 5)));
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let mut state = trees.state();
    state.waiting.insert("g".into(), Waiting { shape: None, origin: Origin::Other });
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    let commit = commit.unwrap_or_default();
    assert!(matches!(&commit[0], op::Op::CreateDir(create) if create.node_id == "p"), "{commit:?} {:?}", trees.waiting());
    assert!(commit.contains(&mv("f", "p", "b")), "{commit:?}");
}

#[test]
fn a_store_the_descendant_loop_left_waiting_syncs_after_the_upgrade() {
    let row = || {
        trees(&[
            ("p", Some(dir(ROOT, "p")), Some(dir(ROOT, "p")), None),
            ("t", None, None, Some(trashed(dir(ROOT, "t")))),
            ("f", Some(file("p", "a", 1)), Some(file("p", "a", 1)), Some(trashed(file("t", "c", 1)))),
        ])
    };
    let mut before = row();
    before.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    before.settle(&Default::default());
    let mut state = before.state();
    state.waiting.insert("f".into(), Waiting { shape: Some(file("t", "c", 1)), origin: Origin::Other });
    state.waiting.insert("p".into(), Waiting { shape: None, origin: Origin::Other });
    let mut upgraded = row();
    upgraded.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    upgraded.restore(state).unwrap();
    upgraded.rules_scanned();
    upgraded.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut upgraded, &mut ids).map(|commit| ops(&commit));
    assert!(matches!(commit.as_deref(), Some([op::Op::CreateDir(create), moved]) if create.node_id == "p" && *moved == mv("f", "p", "b")), "{commit:?}");
    assert_eq!(upgraded.state().unsent_creates, ["p".to_string()].into());
}

fn held_elsewhere_in_a_lost_folder() -> Trees {
    trees(&[
        ("p", Some(dir(ROOT, "p")), Some(dir(ROOT, "p")), None),
        ("f", Some(file("p", "a", 1)), Some(file("p", "a", 1)), Some(remote(file(ROOT, "c", 1)))),
    ])
}

#[test]
fn a_resend_edited_before_it_is_sent_is_created_where_the_server_held_it() {
    let (mut trees, mut commits, mut ids) = renamed_elsewhere_then_lost();
    trees.set_local("f".into(), local(file(ROOT, "a", 2), 1));
    trees.settle(commits.aliases());
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
    trees.settle(commits.aliases());
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
    trees.settle(commits.aliases());
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
    trees.forget_lost(Vec::new(), Vec::new(), Vec::new(), Vec::new(), untaken, Default::default(), &Default::default(), 0, 0);
    trees.settle(&Default::default());
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
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.set_remote("m".into(), remote(dir(ROOT, "y")));
    trees.settle(&Default::default());
    let state = CommitsState { cap: Some(1), ..Default::default() };
    let (mut commits, mut ids) = (Commits::from_state(state).unwrap(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(create)] if create.node_id == "f" && create.parent_id == "q" && create.name == "b"));
    trees.set_remote_made_here("f".into(), remote_at(file("q", "b", 1), 4));
    commits.received(result(4, vec![outcome(op_result::Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle(commits.aliases());
    assert_eq!(trees.waiting().get("f").unwrap().origin, Origin::Commit(commit.device_seq));
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), untaken, Default::default(), &Default::default(), commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle(commits.aliases());
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
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.set_remote("d".into(), remote(dir(ROOT, "b")));
    trees.settle(&Default::default());
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
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.set_remote("m".into(), remote(dir(ROOT, "y")));
    trees.set_remote("g".into(), remote(dir("m", "b")));
    trees.settle(&Default::default());
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
    let records = trees.remote_records(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), records, &Default::default(), 0, 0);
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 0).cloned().unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(create)] if create.name == "b"));
    let own = RemoteNode { version_id: "resent".into(), ..remote(file(ROOT, "b", 1)) };
    trees.set_remote_made_here("f".into(), own);
    if let Some(name) = peer_name {
        let peer = RemoteNode { version_id: "resent".into(), ..remote_at(file(ROOT, name, 1), 2) };
        trees.set_remote("f".into(), peer);
    }
    commits.received(result(1, vec![outcome(op_result::Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, if peer_name.is_some() { 2 } else { 1 }));
    trees.settle(commits.aliases());
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
    let records = trees.remote_records(|seq| commits.lost_here(seq));
    assert_eq!(records.get("f").map(|record| record.node.shape.name.as_str()), Some("c"));
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, moved, untaken, records, &Default::default(), commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle(commits.aliases());
    let commit = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 0).cloned().unwrap();
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
    let records = trees.remote_records(|seq| commits.lost_here(seq));
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, moved, untaken, records, &Default::default(), commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle(commits.aliases());
    let commit = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 0).cloned().unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(create)] if create.name == "b"));
    assert!(trees.state().lost_positions.contains_key("f"));
    assert!(!trees.state().resends.contains("f"));
    let own = RemoteNode { version_id: "resent-again".into(), ..remote(file(ROOT, "b", 1)) };
    trees.set_remote_made_here("f".into(), own);
    let peer = RemoteNode { version_id: "resent-again".into(), ..remote_at(file(ROOT, "c", 1), 2) };
    trees.set_remote("f".into(), peer);
    commits.received(result(1, vec![outcome(op_result::Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle(commits.aliases());
    let class = trees.class("f");
    let next = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 2).cloned().map(|commit| ops(&commit));
    assert_eq!(next, None, "a peer rename during repeated recovery must remain remote, not be undone: {class:?}");
    effects_for(&mut trees, &mut ids, "f");
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "c", 1)));
    assert!(trees.state().lost_positions.is_empty());
}

#[test]
fn a_resend_another_device_trashes_before_its_acknowledgement_is_not_revived() {
    let mut trees = trees(&[synced("f", file(ROOT, "a", 1))]);
    trees.set_remote("f".into(), remote_at(file(ROOT, "b", 1), 2));
    let synced = trees.taken_from_others(|_| false);
    let records = trees.remote_records(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), records, &Default::default(), 0, 0);
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 0).cloned().unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 1)]);
    let own = RemoteNode { version_id: "resent".into(), ..remote(file(ROOT, "b", 1)) };
    trees.set_remote_made_here("f".into(), own);
    let deleted = RemoteNode { meta_rev: 2, meta_edit_rev: 1, version_id: "resent".into(), ..trashed(file(ROOT, "b", 1)) };
    trees.set_remote("f".into(), deleted);
    commits.received(result(1, vec![outcome(op_result::Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle(commits.aliases());
    assert_eq!(trees.class("f"), Class::Remote);
    let planned = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 2).cloned().map(|commit| ops(&commit));
    assert_eq!(planned, None);
    effects_for(&mut trees, &mut ids, "f");
    assert!(trees.local().get("f").is_none());
    assert!(trees.state().lost_positions.is_empty());
    assert!(trees.state().lost_creates.is_empty());
    assert!(trees.state().resends.is_empty());
}

#[test]
fn a_resend_another_device_trashes_and_restores_before_its_acknowledgement_takes_the_restored_name() {
    let (mut trees, mut commits, mut ids) = renamed_elsewhere_then_lost();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 1)]);
    trees.set_remote_made_here("f".into(), remote_at(file(ROOT, "b", 1), 3));
    let deleted = RemoteNode { meta_rev: 4, meta_edit_rev: 4, trash_group: "4:0".into(), ..trashed(file(ROOT, "b", 1)) };
    trees.set_remote("f".into(), deleted);
    trees.set_remote("f".into(), remote_at(file(ROOT, "c", 1), 5));
    acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied)]);
    assert_eq!(trees.class("f"), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "f");
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "c", 1)));
    assert!(trees.state().lost_positions.is_empty());
}

#[test]
fn a_file_edited_here_before_a_loss_is_created_where_another_device_renamed_it() {
    let mut trees = trees(&[synced("f", file(ROOT, "a", 1))]);
    trees.set_remote("f".into(), remote_at(file(ROOT, "b", 1), 2));
    trees.set_local("f".into(), local(file(ROOT, "a", 2), 1));
    let synced = trees.taken_from_others(|_| false);
    let untaken = trees.untaken_changes(|_| false);
    let records = trees.remote_records(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), untaken, records, &Default::default(), 0, 0);
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 2)]);
    trees.set_remote_made_here("f".into(), remote(file(ROOT, "b", 2)));
    acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied)]);
    assert_eq!(trees.class("f"), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "f");
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "b", 2)));
    assert!(trees.state().lost_positions.is_empty());
}

#[test]
fn a_file_whose_lost_edit_another_device_renamed_is_created_at_that_name() {
    let mut trees = trees(&[synced("f", file(ROOT, "a", 2))]);
    let renamed = RemoteNode { content_rev: 5, edit_rev: 5, ..remote_at(file(ROOT, "b", 2), 6) };
    trees.set_remote("f".into(), renamed);
    let synced = trees.taken_from_others(|seq| seq == 5);
    let untaken = trees.untaken_changes(|seq| seq == 5);
    let records = trees.remote_records(|seq| seq == 5);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), vec![("f".into(), 5)], synced, Vec::new(), untaken, records, &Default::default(), 0, 0);
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 2)]);
    trees.set_remote_made_here("f".into(), remote(file(ROOT, "b", 2)));
    acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied)]);
    assert_eq!(trees.class("f"), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "f");
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "b", 2)));
    assert!(trees.state().lost_positions.is_empty());
    assert!(trees.state().lost_creates.is_empty());
}

#[test]
fn a_move_this_device_undid_before_a_loss_is_not_sent_again() {
    let mut trees = trees(&[synced("f", file(ROOT, "x", 1))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("f".into(), local(file(ROOT, "y", 1), 1));
    trees.settle(commits.aliases());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", ROOT, "y")]);
    trees.set_remote_made_here("f".into(), remote_at(file(ROOT, "y", 1), 2));
    assert!(commits.own_record(2, &commit.commit_id));
    commits.received(result(2, vec![outcome(op_result::Code::Applied)])).unwrap();
    trees.set_local("f".into(), local(file(ROOT, "x", 1), 1));
    trees.settle(commits.aliases());
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited, moved) = (trees.trashed_here(), trees.edited_here(), trees.moved_here());
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records(|seq| commits.lost_here(seq));
    trees.remove_remote("f");
    trees.forget_lost(trashed, edited, synced, moved, untaken, records, &Default::default(), commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle(commits.aliases());
    assert!(trees.state().lost_positions.is_empty());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("x".to_owned(), 1)]);
}

#[test]
fn a_resend_into_a_lost_folder_the_server_refused_waits_for_that_folder() {
    let mut trees = trees(&[
        synced("p", dir(ROOT, "p")),
        synced("f", file(ROOT, "a", 1)),
    ]);
    trees.set_remote("f".into(), remote_at(file("p", "b", 1), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.remove_remote("p");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 1)]);
    let rejected = |reason: &str| OpResult { reject_reason: reason.into(), ..outcome(op_result::Code::Rejected) };
    acknowledge(&mut commits, &mut trees, vec![rejected("INVALID_NAME"), rejected("NOT_FOUND")]);
    assert!(commits.clear_problem("f"));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(commits.clear_problem("p"));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), 1)]);
}

fn undone_move_then_edit_then_loss(peer: Option<&str>) -> Vec<(String, u8)> {
    let mut trees = trees(&[synced("f", file(ROOT, "x", 1))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("f".into(), local(file(ROOT, "y", 1), 1));
    trees.settle(commits.aliases());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", ROOT, "y")]);
    trees.set_remote_made_here("f".into(), remote_at(file(ROOT, "y", 1), 2));
    assert!(commits.own_record(2, &commit.commit_id));
    commits.received(result(2, vec![outcome(op_result::Code::Applied)])).unwrap();
    if let Some(name) = peer {
        trees.set_remote("f".into(), remote_at(file(ROOT, name, 1), 3));
    }
    trees.set_local_changed("f".into(), local(file(ROOT, "x", 2), 1));
    trees.settle(commits.aliases());
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited, moved) = (trees.trashed_here(), trees.edited_here(), trees.moved_here());
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records(|seq| commits.lost_here(seq));
    trees.remove_remote("f");
    trees.forget_lost(trashed, edited, synced, moved, untaken, records, &Default::default(), commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle(commits.aliases());
    created_at(&build(&mut commits, &mut trees, &mut ids).unwrap())
}

#[test]
fn a_file_edited_after_this_device_undid_its_move_is_created_where_l_holds_it_after_a_loss() {
    assert_eq!(undone_move_then_edit_then_loss(None), [("x".to_owned(), 2)]);
}

#[test]
fn a_file_another_device_renamed_after_this_device_undid_its_move_is_created_at_that_name() {
    assert_eq!(undone_move_then_edit_then_loss(Some("z")), [("z".to_owned(), 2)]);
}

#[test]
fn a_resend_whose_lost_folder_is_left_out_of_the_planned_creates_is_not_created_at_its_old_name() {
    let mut trees = trees(&[
        synced("q", dir(ROOT, "q")),
        synced("f", file(ROOT, "a", 1)),
        ("g", Some(file(ROOT, "g", 2)), None, None),
        ("h", Some(file(ROOT, "h", 3)), None, None),
    ]);
    trees.set_remote("f".into(), remote_at(file("q", "b", 1), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.remove_remote("q");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.settle(&Default::default());
    let state = koidrive_engine::state::CommitsState {
        cap: Some(1),
        ..Commits::default().state()
    };
    let (mut commits, mut ids) = (Commits::from_state(state).unwrap(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(!created_at(&commit).contains(&("a".to_owned(), 1)), "{:?}", ops(&commit));
}

#[test]
fn a_folder_another_device_renamed_before_a_loss_is_created_without_the_name_it_was_pending_for() {
    let mut trees = trees(&[synced("f", dir(ROOT, "a"))]);
    trees.set_synced_mark("f".into(), Some("x".into()));
    trees.set_remote("f".into(), remote_at(dir(ROOT, "b"), 2));
    let synced = trees.taken_from_others(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), Default::default(), &Default::default(), 0, 0);
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateDir(create)] if create.name == "b" && create.pending_for.is_empty()),
        "{:?}",
        ops(&commit)
    );
}
