// SPDX-License-Identifier: Apache-2.0

fn trashed_at(shape: Shape, seq: u64) -> RemoteNode {
    RemoteNode {
        trashed: true,
        trash_group: format!("{seq}:2"),
        ..remote_at(shape, seq)
    }
}

#[test]
fn a_move_out_of_the_trash_sends_a_create_onto_a_member_its_user_removed_after_trashing_it_again() {
    let mut trees = trees(&[
        ("p", None, Some(dir(ROOT, "p")), Some(trashed(dir(ROOT, "p")))),
        ("a", Some(dir(ROOT, "a")), Some(dir("p", "a")), Some(trashed(dir("p", "a")))),
        ("g", None, Some(file("a", "g", 1)), Some(trashed(file("a", "g", 1)))),
        ("h", Some(file("a", "g", 2)), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let create = op::Op::CreateFile(proto::CreateFile { made_over: Vec::new(), source_version_id: String::new(),
        node_id: "h".into(),
        parent_id: "a".into(),
        name: "g".into(),
        content: Some(content(2)),
        ..Default::default()
    });
    assert_eq!(
        ops(&commit),
        [mv("a", ROOT, "a"), mv("g", "a", "g"), delete("g"), create]
    );

    trees.set_remote_made_here("a".into(), remote_at(dir(ROOT, "a"), 2));
    trees.set_remote_made_here("g".into(), trashed_at(file("a", "g", 1), 2));
    trees.set_remote_made_here("h".into(), remote_at(file("a", "g", 2), 2));
    use op_result::Code;
    commits
        .received(result(2, vec![outcome(Code::Applied); 4]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle(commits.aliases());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_delete_its_own_move_out_of_the_trash_superseded_is_sent_again() {
    let mut trees = trees(&[
        ("a", Some(dir(ROOT, "A")), Some(dir(ROOT, "a")), Some(remote(dir(ROOT, "a")))),
        ("g", None, Some(file("a", "g", 1)), Some(remote(file("a", "g", 1)))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("a", ROOT, "A"), delete("g")]);

    let at = |node: RemoteNode, rev: u64| RemoteNode {
        meta_rev: rev,
        ..node
    };
    trees.set_remote("a".into(), at(trashed(dir(ROOT, "a")), 2));
    trees.set_remote("g".into(), at(trashed(file("a", "g", 1)), 2));
    trees.set_remote_made_here("a".into(), remote_at(dir(ROOT, "A"), 3));
    trees.set_remote_made_here("g".into(), remote_at(file("a", "g", 1), 3));
    use op_result::Code;
    let results = vec![outcome(Code::Applied), outcome(Code::Superseded)];
    commits.received(result(3, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle(commits.aliases());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("g")]);
}

#[test]
fn a_member_another_device_edits_after_its_superseded_delete_is_downloaded() {
    let mut trees = trees(&[
        ("a", Some(dir(ROOT, "A")), Some(dir(ROOT, "a")), Some(remote(dir(ROOT, "a")))),
        ("g", None, Some(file("a", "g", 1)), Some(remote(file("a", "g", 1)))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("a", ROOT, "A"), delete("g")]);

    let at = |node: RemoteNode, rev: u64| RemoteNode {
        meta_rev: rev,
        ..node
    };
    trees.set_remote("a".into(), at(trashed(dir(ROOT, "a")), 8));
    trees.set_remote("g".into(), at(trashed(file("a", "g", 1)), 8));
    trees.set_remote_made_here("a".into(), remote_at(dir(ROOT, "A"), 9));
    trees.set_remote_made_here("g".into(), remote_at(file("a", "g", 1), 9));
    let edited = RemoteNode {
        content_rev: 10,
        edit_rev: 10,
        ..remote_at(file("a", "g", 2), 9)
    };
    trees.set_remote("g".into(), edited);
    use op_result::Code;
    let results = vec![outcome(Code::Applied), outcome(Code::Superseded)];
    commits.received(result(9, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 10));
    trees.settle(commits.aliases());
    assert_eq!(trees.class(&"g".into()), Class::Remote);
    let sent = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(!sent.unwrap_or_default().contains(&delete("g")));
}

#[test]
fn a_rename_out_of_the_trash_sends_a_create_onto_a_member_its_user_removed_after_trashing_it_again()
{
    let mut trees = trees(&[
        ("t", Some(dir(ROOT, "T2")), Some(dir(ROOT, "T")), Some(trashed(dir(ROOT, "T")))),
        ("g", None, Some(file("t", "g", 1)), Some(trashed(file("t", "g", 1)))),
        ("x", Some(file("t", "g", 2)), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let create = op::Op::CreateFile(proto::CreateFile { made_over: Vec::new(), source_version_id: String::new(),
        node_id: "x".into(),
        parent_id: "t".into(),
        name: "g".into(),
        content: Some(content(2)),
        ..Default::default()
    });
    assert_eq!(
        ops(&commit),
        [mv("t", ROOT, "T2"), mv("g", "t", "g"), delete("g"), create]
    );

    trees.set_remote_made_here("t".into(), remote_at(dir(ROOT, "T2"), 2));
    trees.set_remote_made_here("g".into(), trashed_at(file("t", "g", 1), 2));
    trees.set_remote_made_here("x".into(), remote_at(file("t", "g", 2), 2));
    use op_result::Code;
    commits
        .received(result(2, vec![outcome(Code::Applied); 4]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle(commits.aliases());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

fn listed_after_renaming_out_of_the_trash(
    listed: [RemoteNode; 2],
    results: Vec<OpResult>,
    result_seq: u64,
    answer_first: bool,
) -> (Trees, Option<Vec<op::Op>>) {
    let mut trees = trees(&[
        ("t", Some(dir(ROOT, "T2")), Some(dir(ROOT, "T")), Some(trashed(dir(ROOT, "T")))),
        ("g", None, Some(file("t", "g", 1)), Some(trashed(file("t", "g", 1)))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [mv("t", ROOT, "T2"), mv("g", "t", "g"), delete("g")]
    );
    let mut answer = Some(result(result_seq, results));
    if answer_first {
        commits.received(answer.take().unwrap()).unwrap();
    }
    let removed = trees.removed_here();
    let [t, g] = listed;
    trees.set_listed_remote("t".into(), t);
    trees.set_listed_remote("g".into(), g);
    trees.listed_removals(removed, |seq| commits.listed_here(seq));
    if let Some(answer) = answer {
        commits.received(answer).unwrap();
    }
    commits.acknowledge(&mut trees, result_seq);
    trees.settle(commits.aliases());
    let sent = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    (trees, sent)
}

#[test]
fn a_listing_after_a_rename_out_of_the_trash_was_answered_keeps_the_member_its_user_removed_trashed()
 {
    use op_result::Code;
    let listed = [
        remote_at(dir(ROOT, "T2"), 2),
        trashed_at(file("t", "g", 1), 2),
    ];
    let (trees, sent) =
        listed_after_renaming_out_of_the_trash(listed, vec![outcome(Code::Applied); 3], 2, true);
    assert_eq!(sent, None);
    assert_eq!(trees.class(&"g".into()), Class::InSync);
}

#[test]
fn a_listing_of_another_devices_rename_out_of_the_trash_downloads_the_member() {
    use op_result::Code;
    for answer_first in [true, false] {
        let listed = [
            remote_at(dir(ROOT, "T2"), 3),
            remote_at(file("t", "g", 1), 2),
        ];
        let results = vec![
            outcome(Code::Applied),
            outcome(Code::Applied),
            outcome(Code::Superseded),
        ];
        let (trees, sent) =
            listed_after_renaming_out_of_the_trash(listed, results, 3, answer_first);
        assert_eq!(
            trees.class(&"g".into()),
            Class::Remote,
            "answer first: {answer_first}"
        );
        assert!(
            !sent.unwrap_or_default().contains(&delete("g")),
            "answer first: {answer_first}"
        );
    }
}

#[test]
fn a_listing_of_a_member_another_device_restored_and_edited_after_this_devices_rename_downloads_it()
{
    use op_result::Code;
    let restored = RemoteNode {
        content_rev: 4,
        edit_rev: 4,
        ..remote_at(file("t", "g", 2), 3)
    };
    let listed = [remote_at(dir(ROOT, "T2"), 2), restored];
    let (trees, sent) =
        listed_after_renaming_out_of_the_trash(listed, vec![outcome(Code::Applied); 3], 2, true);
    assert_eq!(trees.class(&"g".into()), Class::Remote);
    assert!(!sent.unwrap_or_default().contains(&delete("g")));
}

#[test]
fn an_edit_another_device_made_before_the_revival_survives_the_superseded_delete() {
    let mut trees = trees(&[
        ("a", Some(dir(ROOT, "A")), Some(dir(ROOT, "a")), Some(remote(dir(ROOT, "a")))),
        ("g", None, Some(file("a", "g", 1)), Some(remote(file("a", "g", 1)))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("a", ROOT, "A"), delete("g")]);
    let edited = RemoteNode {
        content_rev: 8,
        edit_rev: 8,
        ..remote_at(file("a", "g", 2), 1)
    };
    trees.set_remote("g".into(), edited.clone());
    trees.settle(commits.aliases());
    let trashed_at_9 = |node: RemoteNode| RemoteNode {
        trashed: true,
        trash_group: "2:0".into(),
        meta_rev: 9,
        ..node
    };
    trees.set_remote("a".into(), trashed_at_9(remote(dir(ROOT, "a"))));
    trees.set_remote("g".into(), trashed_at_9(edited.clone()));
    trees.settle(commits.aliases());
    trees.set_remote_made_here("a".into(), remote_at(dir(ROOT, "A"), 10));
    trees.set_remote_made_here("g".into(), RemoteNode { meta_rev: 10, ..edited });
    use op_result::Code;
    let results = vec![outcome(Code::Applied), outcome(Code::Superseded)];
    commits.received(result(10, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 10));
    trees.settle(commits.aliases());
    assert_eq!(trees.class(&"g".into()), Class::Remote);
    let sent = build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit));
    assert!(!sent.unwrap_or_default().contains(&delete("g")));
}

#[test]
fn a_listing_keeps_an_edit_another_device_made_before_this_devices_rename_out_of_the_trash() {
    use op_result::Code;
    let edited = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        ..remote_at(file("t", "g", 2), 2)
    };
    let listed = [remote_at(dir(ROOT, "T2"), 4), edited];
    let results = vec![
        outcome(Code::Applied),
        outcome(Code::Applied),
        outcome(Code::Superseded),
    ];
    let (trees, sent) = listed_after_renaming_out_of_the_trash(listed, results, 4, true);
    assert_eq!(trees.class(&"g".into()), Class::Remote);
    assert!(!sent.unwrap_or_default().contains(&delete("g")));
}

#[test]
fn a_listing_after_a_rename_sent_before_another_devices_trash_deletes_the_member_removed_here_again()
 {
    use op_result::Code;
    let mut trees = trees(&[
        (
            "t",
            Some(dir(ROOT, "T2")),
            Some(dir(ROOT, "T")),
            Some(remote(dir(ROOT, "T"))),
        ),
        (
            "g",
            Some(file("t", "g", 1)),
            Some(file("t", "g", 1)),
            Some(remote(file("t", "g", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("t", ROOT, "T2")]);
    trees.set_remote("t".into(), trashed_at(dir(ROOT, "T"), 2));
    trees.set_remote("g".into(), trashed_at(file("t", "g", 1), 2));
    trees.remove_local(&"g".into());
    trees.settle(commits.aliases());
    assert_eq!(trees.removed_here(), BTreeMap::from([("g".into(), 2)]));
    commits
        .received(result(3, vec![outcome(Code::Applied)]))
        .unwrap();
    let removed = trees.removed_here();
    trees.set_listed_remote("t".into(), remote_at(dir(ROOT, "T2"), 3));
    trees.set_listed_remote("g".into(), remote_at(file("t", "g", 1), 3));
    trees.listed_removals(removed, |seq| commits.listed_here(seq));
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle(commits.aliases());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("g")]);
}

#[test]
fn a_move_out_of_the_trash_goes_even_while_a_member_it_carries_out_waits_for_its_folders_own_move()
{
    let mut trees = trees(&[
        (
            "a",
            Some(dir(ROOT, "a1")),
            Some(dir(ROOT, "a")),
            Some(trashed(dir(ROOT, "a"))),
        ),
        (
            "p",
            Some(dir("a", "p2")),
            Some(dir("a", "p")),
            Some(trashed(dir("a", "p"))),
        ),
        ("b", None, Some(dir("p", "b")), Some(trashed(dir("p", "b")))),
        (
            "c",
            Some(dir("n", "c")),
            Some(dir("b", "c")),
            Some(trashed(dir("b", "c"))),
        ),
        ("n", Some(dir("a", "n")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let sent = ops(&commit);
    assert!(sent.contains(&mv("a", ROOT, "a1")), "{sent:?}");
    assert!(sent.contains(&delete("b")), "{sent:?}");
    assert!(sent.contains(&mv("c", "n", "c")), "{sent:?}");
}

#[test]
fn a_rename_out_of_the_trash_trashes_again_only_the_members_its_user_removed() {
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
        ("n", None, None, Some(trashed(file("t", "n", 3)))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [mv("t", ROOT, "T2"), mv("g", "t", "g"), delete("g")]
    );
}

#[test]
fn a_rename_out_of_the_trash_leaves_a_member_another_device_restored_after_its_plan_where_it_was_put()
 {
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
        (
            "k",
            Some(dir(ROOT, "K")),
            Some(dir(ROOT, "K")),
            Some(remote(dir(ROOT, "K"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [mv("t", ROOT, "T2"), mv("g", "t", "g"), delete("g")]
    );
    assert!(commit.require_head);

    commits
        .failed(&mut trees, &refusal(error::Code::HeadMoved, 0))
        .unwrap();
    trees.set_remote("g".into(), remote_at(file("k", "g", 1), 3));
    trees.settle(commits.aliases());
    let again = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&again), [mv("t", ROOT, "T2")]);
}

#[test]
fn a_rename_out_of_the_trash_keeps_a_folder_its_user_removed_live_while_it_holds_a_member_another_device_edited()
 {
    let edited = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 3,
        ..trashed(file("s", "g", 2))
    };
    let mut trees = trees(&[
        (
            "t",
            Some(dir(ROOT, "T2")),
            Some(dir(ROOT, "T")),
            Some(trashed(dir(ROOT, "T"))),
        ),
        ("s", None, Some(dir("t", "s")), Some(trashed(dir("t", "s")))),
        ("g", None, Some(file("s", "g", 1)), Some(edited)),
        (
            "h",
            None,
            Some(file("s", "h", 1)),
            Some(trashed(file("s", "h", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [mv("t", ROOT, "T2"), mv("h", "s", "h"), delete("h")]
    );
}

#[test]
fn a_rename_out_of_the_trash_keeps_a_folder_its_user_removed_live_while_it_holds_a_member_this_device_never_held()
 {
    let mut trees = trees(&[
        (
            "t",
            Some(dir(ROOT, "T2")),
            Some(dir(ROOT, "T")),
            Some(trashed(dir(ROOT, "T"))),
        ),
        ("s", None, Some(dir("t", "s")), Some(trashed(dir("t", "s")))),
        (
            "g",
            None,
            Some(file("s", "g", 1)),
            Some(trashed(file("s", "g", 1))),
        ),
        ("n", None, None, Some(trashed(file("s", "n", 3)))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [mv("t", ROOT, "T2"), mv("g", "s", "g"), delete("g")]
    );
}

#[test]
fn a_rename_out_of_the_trash_keeps_a_member_another_device_edited_before_trashing_it() {
    let edited = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 3,
        ..trashed(file("t", "g", 2))
    };
    let folder = RemoteNode {
        meta_rev: 3,
        ..trashed(dir(ROOT, "T"))
    };
    let mut trees = trees(&[
        (
            "t",
            Some(dir(ROOT, "T2")),
            Some(dir(ROOT, "T")),
            Some(folder),
        ),
        ("g", None, Some(file("t", "g", 1)), Some(edited)),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("t", ROOT, "T2")]);
}
fn lost_after_renaming_out_of_the_trash(listed: RemoteNode) -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[
        (
            "p",
            Some(dir(ROOT, "p")),
            Some(dir(ROOT, "p")),
            Some(remote(dir(ROOT, "p"))),
        ),
        ("t", None, None, Some(trashed(dir(ROOT, "t")))),
        (
            "f",
            Some(file("p", "a", 1)),
            Some(file("p", "a", 1)),
            Some(trashed(file("t", "c", 1))),
        ),
    ]);
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", "p", "b")]);
    trees.set_remote_made_here("f".into(), remote_at(file("p", "b", 1), 5));
    commits
        .received(result(5, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle(commits.aliases());
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (deleted, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    trees.set_listed_remote("f".into(), listed);
    trees.forget_lost(
        deleted,
        edited,
        synced,
        moved,
        Vec::new(),
        Default::default(),
        &Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle(commits.aliases());
    (trees, commits, ids)
}

#[test]
fn a_rename_out_of_the_trash_the_server_lost_is_sent_again() {
    let (mut trees, mut commits, mut ids) =
        lost_after_renaming_out_of_the_trash(trashed(file("t", "c", 1)));
    assert_eq!(effects_for(&mut trees, &mut ids, "f"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", "p", "b")]);
    trees.set_remote_made_here("f".into(), remote_at(file("p", "b", 1), 6));
    commits
        .received(result(6, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 6));
    trees.settle(commits.aliases());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(effects_for(&mut trees, &mut ids, "f"), []);
    assert_eq!(trees.synced().live(&"f".into()), Some(&file("p", "b", 1)));
    assert!(trees.waiting().is_empty());
}

#[test]
fn a_rename_out_of_the_trash_the_server_lost_yields_to_a_later_trash() {
    let (mut trees, mut commits, mut ids) =
        lost_after_renaming_out_of_the_trash(trashed_at(file("t", "c", 1), 6));
    assert!(matches!(
        &effects_for(&mut trees, &mut ids, "f")[..],
        [Effect::RemoveFile { .. }]
    ));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_folder_rename_out_of_the_trash_the_server_lost_is_sent_again() {
    let mut trees = trees(&[
        (
            "t",
            Some(dir(ROOT, "T2")),
            Some(dir(ROOT, "T")),
            Some(trashed_at(dir(ROOT, "T"), 1)),
        ),
        (
            "g",
            Some(file("t", "g", 1)),
            Some(file("t", "g", 1)),
            Some(trashed_at(file("t", "g", 1), 1)),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("t", ROOT, "T2")]);
    trees.set_remote_made_here("t".into(), remote_at(dir(ROOT, "T2"), 5));
    trees.set_remote_made_here("g".into(), remote_at(file("t", "g", 1), 5));
    commits
        .received(result(5, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle(commits.aliases());
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (deleted, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    trees.set_listed_remote("t".into(), trashed_at(dir(ROOT, "T"), 1));
    trees.set_listed_remote("g".into(), trashed_at(file("t", "g", 1), 1));
    trees.forget_lost(
        deleted,
        edited,
        synced,
        moved,
        Vec::new(),
        Default::default(),
        &Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle(commits.aliases());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("t", ROOT, "T2")]);
    assert_eq!(effects_for(&mut trees, &mut ids, "t"), []);
}
fn lose_and_list(trees: &mut Trees, commits: &mut Commits, listed: &[(&str, RemoteNode)]) {
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (deleted, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    for (id, node) in listed {
        trees.set_listed_remote((*id).into(), node.clone());
    }
    trees.forget_lost(
        deleted,
        edited,
        synced,
        moved,
        Vec::new(),
        Default::default(),
        &Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle(commits.aliases());
}

fn acknowledge_made_here(
    trees: &mut Trees,
    commits: &mut Commits,
    seq: u64,
    made: &[(&str, RemoteNode)],
) {
    for (id, node) in made {
        trees.set_remote_made_here((*id).into(), node.clone());
    }
    commits
        .received(result(
            seq,
            vec![outcome(op_result::Code::Applied); made.len()],
        ))
        .unwrap();
    assert!(commits.acknowledge(trees, seq));
    trees.settle(commits.aliases());
}

fn renamed_in_place_out_of_the_trash() -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[
        (
            "p",
            Some(dir(ROOT, "p")),
            Some(dir(ROOT, "p")),
            Some(remote(dir(ROOT, "p"))),
        ),
        (
            "f",
            Some(file("p", "a", 1)),
            Some(file("p", "a", 1)),
            Some(remote(file("p", "a", 1))),
        ),
    ]);
    trees.set_remote("f".into(), trashed_at(file("p", "a", 1), 2));
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", "p", "b")]);
    acknowledge_made_here(
        &mut trees,
        &mut commits,
        3,
        &[("f", remote_at(file("p", "b", 1), 3))],
    );
    (trees, commits, ids)
}

#[test]
fn a_rename_in_place_out_of_the_trash_the_server_lost_is_sent_again_over_the_same_trash() {
    let (mut trees, mut commits, mut ids) = renamed_in_place_out_of_the_trash();
    lose_and_list(
        &mut trees,
        &mut commits,
        &[("f", trashed_at(file("p", "a", 1), 2))],
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", "p", "b")]);
}

#[test]
fn a_fresh_trash_at_the_reused_seq_is_not_undone() {
    let fresh = RemoteNode {
        trash_group: "commit-of-z:0".into(),
        ..trashed(file("t", "c", 1))
    };
    let (mut trees, mut commits, mut ids) = lost_after_renaming_out_of_the_trash(fresh);
    let effects = effects_for(&mut trees, &mut ids, "f");
    assert_eq!(
        build(&mut commits, &mut trees, &mut ids).map(|c| ops(&c)),
        None
    );
    assert!(matches!(&effects[..], [Effect::RemoveFile { .. }]));
}

#[test]
fn a_fresh_trash_in_place_at_the_reused_seq_is_not_undone() {
    let (mut trees, mut commits, mut ids) = renamed_in_place_out_of_the_trash();
    let fresh = RemoteNode {
        trash_group: "commit-of-z:0".into(),
        ..trashed_at(file("p", "a", 1), 2)
    };
    lose_and_list(&mut trees, &mut commits, &[("f", fresh)]);
    let effects = effects_for(&mut trees, &mut ids, "f");
    assert_eq!(
        build(&mut commits, &mut trees, &mut ids).map(|c| ops(&c)),
        None
    );
    assert!(matches!(&effects[..], [Effect::RemoveFile { .. }]));
}

#[test]
fn a_fresh_trash_of_the_folder_at_the_reused_seq_is_not_undone() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir(ROOT, "p")),
            Some(dir(ROOT, "p")),
            Some(remote(dir(ROOT, "p"))),
        ),
        ("t", None, None, Some(trashed(dir(ROOT, "t")))),
        (
            "f",
            Some(file("p", "a", 1)),
            Some(file("p", "a", 1)),
            Some(trashed(file("t", "c", 1))),
        ),
    ]);
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", "p", "b")]);
    acknowledge_made_here(
        &mut trees,
        &mut commits,
        5,
        &[("f", remote_at(file("p", "b", 1), 5))],
    );
    let fresh_t = RemoteNode {
        trash_group: "commit-of-z:0".into(),
        ..trashed(dir(ROOT, "t"))
    };
    let fresh_f = RemoteNode {
        trash_group: "commit-of-z:0".into(),
        ..trashed(file("t", "c", 1))
    };
    lose_and_list(&mut trees, &mut commits, &[("t", fresh_t), ("f", fresh_f)]);
    let effects = effects_for(&mut trees, &mut ids, "f");
    assert_eq!(
        build(&mut commits, &mut trees, &mut ids).map(|c| ops(&c)),
        None
    );
    assert!(matches!(&effects[..], [Effect::RemoveFile { .. }]));
}

#[test]
fn a_rename_back_after_a_rename_out_of_the_trash_the_server_lost_is_sent() {
    let (mut trees, mut commits, mut ids) = renamed_in_place_out_of_the_trash();
    trees.set_local_changed("f".into(), local(file("p", "a", 1), 2));
    trees.settle(commits.aliases());
    lose_and_list(
        &mut trees,
        &mut commits,
        &[("f", trashed_at(file("p", "a", 1), 2))],
    );
    let effects = effects_for(&mut trees, &mut ids, "f");
    let commit = build(&mut commits, &mut trees, &mut ids).map(|c| ops(&c));
    assert_eq!(effects, []);
    assert_eq!(commit, Some(vec![mv("f", "p", "a")]));
}

#[test]
fn a_held_rename_back_after_a_rename_out_of_the_trash_the_server_lost_is_sent_again() {
    let (mut trees, mut commits, mut ids) = renamed_in_place_out_of_the_trash();
    trees.set_local_changed("f".into(), local(file("p", "a", 1), 2));
    trees.settle(commits.aliases());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", "p", "a")]);
    acknowledge_made_here(
        &mut trees,
        &mut commits,
        4,
        &[("f", remote_at(file("p", "a", 1), 4))],
    );
    lose_and_list(
        &mut trees,
        &mut commits,
        &[("f", trashed_at(file("p", "a", 1), 2))],
    );
    let effects = effects_for(&mut trees, &mut ids, "f");
    let commit = build(&mut commits, &mut trees, &mut ids).map(|c| ops(&c));
    assert_eq!(effects, []);
    assert_eq!(commit, Some(vec![mv("f", "p", "a")]));
}

#[test]
fn a_rename_back_after_the_recovery_of_a_lost_rename_out_of_the_trash_is_sent() {
    let (mut trees, mut commits, mut ids) = renamed_in_place_out_of_the_trash();
    lose_and_list(
        &mut trees,
        &mut commits,
        &[("f", trashed_at(file("p", "a", 1), 2))],
    );
    trees.set_local_changed("f".into(), local(file("p", "a", 1), 2));
    trees.settle(commits.aliases());
    let effects = effects_for(&mut trees, &mut ids, "f");
    let commit = build(&mut commits, &mut trees, &mut ids).map(|c| ops(&c));
    assert_eq!(effects, []);
    assert_eq!(commit, Some(vec![mv("f", "p", "a")]));
}

fn lost_after_a_later_trash_this_device_had_not_taken(
    later: &[(&str, RemoteNode)],
    listed: &[(&str, RemoteNode)],
) -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[
        (
            "p",
            Some(dir(ROOT, "p")),
            Some(dir(ROOT, "p")),
            Some(remote(dir(ROOT, "p"))),
        ),
        ("t", None, None, Some(trashed(dir(ROOT, "t")))),
        (
            "f",
            Some(file("p", "a", 1)),
            Some(file("p", "a", 1)),
            Some(trashed(file("t", "c", 1))),
        ),
    ]);
    trees.set_local_changed("f".into(), local(file("p", "b", 1), 2));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", "p", "b")]);
    acknowledge_made_here(
        &mut trees,
        &mut commits,
        5,
        &[("f", remote_at(file("p", "b", 1), 5))],
    );
    for (id, node) in later {
        trees.set_remote((*id).into(), node.clone());
    }
    trees.settle(commits.aliases());
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (deleted, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let trashed_again: Vec<(NodeId, Untaken)> = (later.iter())
        .map(|(id, _)| ((*id).into(), Untaken::Delete))
        .collect();
    assert_eq!(untaken.len(), later.len());
    assert!(trashed_again.iter().all(|change| untaken.contains(change)));
    for (id, node) in listed {
        trees.set_listed_remote((*id).into(), node.clone());
    }
    trees.forget_lost(
        deleted,
        edited,
        synced,
        moved,
        untaken,
        Default::default(),
        &Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle(commits.aliases());
    (trees, commits, ids)
}

#[test]
fn a_lost_rename_out_of_the_trash_yields_to_a_later_trash_this_device_had_not_taken() {
    let (mut trees, mut commits, mut ids) = lost_after_a_later_trash_this_device_had_not_taken(
        &[("f", trashed_at(file("p", "b", 1), 6))],
        &[("f", trashed(file("t", "c", 1)))],
    );
    let effects = effects_for(&mut trees, &mut ids, "f");
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(matches!(&effects[..], [Effect::RemoveFile { .. }]));
}

#[test]
fn a_lost_rename_out_of_the_trash_yields_to_a_later_trash_of_its_folder_this_device_had_not_taken()
{
    let (mut trees, mut commits, mut ids) = lost_after_a_later_trash_this_device_had_not_taken(
        &[
            ("p", trashed_at(dir(ROOT, "p"), 6)),
            ("f", trashed_at(file("p", "b", 1), 6)),
        ],
        &[
            ("p", remote(dir(ROOT, "p"))),
            ("f", trashed(file("t", "c", 1))),
        ],
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}
