// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_late_acknowledgement_keeps_the_position_s_took_after_the_commit_was_built() {
    a_late_acknowledgement_keeps_the_position_s_took_after_the_commit(false, false);
}

#[test]
fn a_late_acknowledgement_after_a_restart_keeps_the_position_s_took_after_the_commit_was_built() {
    a_late_acknowledgement_keeps_the_position_s_took_after_the_commit(true, false);
}

#[test]
fn a_late_acknowledgement_renamed_by_the_server_keeps_the_position_s_took_after_the_commit_was_built()
 {
    a_late_acknowledgement_keeps_the_position_s_took_after_the_commit(false, true);
}

#[test]
fn an_edit_of_a_node_waiting_behind_a_problem_node_is_uploaded() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "m",
            Some(file(ROOT, "d", 2)),
            Some(file(ROOT, "c", 2)),
            Some(remote(file(ROOT, "c", 2))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("m", ROOT, "d"), mv("n", ROOT, "b")]);
    let rejected = OpResult {
        reject_reason: "NAME_INVALID".into(),
        ..outcome(op_result::Code::Rejected)
    };
    trees.set_remote("m".into(), remote_at(file(ROOT, "e", 2), 3));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 4));
    trees.settle(commits.aliases());
    let results = vec![rejected, outcome(op_result::Code::Applied)];
    commits.received(result(2, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle(commits.aliases());
    trees.set_local("n".into(), local(file(ROOT, "c", 9), 1));
    trees.settle(commits.aliases());
    let commit = build(&mut commits, &mut trees, &mut ids);
    assert_eq!(commit.as_ref().map(ops), Some(vec![update("n", 9)]));
}

#[test]
fn an_edit_of_a_waiting_node_r_moved_on_from_is_uploaded() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "m",
            Some(file(ROOT, "d", 2)),
            Some(file(ROOT, "c", 2)),
            Some(remote(file(ROOT, "c", 2))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("m", ROOT, "d"), mv("n", ROOT, "b")]);
    let rejected = OpResult {
        reject_reason: "NAME_INVALID".into(),
        ..outcome(op_result::Code::Rejected)
    };
    trees.set_remote("m".into(), remote_at(file(ROOT, "e", 2), 3));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 4));
    trees.settle(commits.aliases());
    trees.set_remote("n".into(), remote_at(file(ROOT, "x", 1), 5));
    trees.settle(commits.aliases());
    let results = vec![rejected, outcome(op_result::Code::Applied)];
    commits.received(result(2, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle(commits.aliases());
    trees.set_local("n".into(), local(file(ROOT, "c", 9), 1));
    trees.settle(commits.aliases());
    let mut sent = Vec::new();
    for seq in 10..14 {
        if let Some(commit) = build(&mut commits, &mut trees, &mut ids) {
            sent.extend(ops(&commit));
            let applied = vec![outcome(op_result::Code::Applied); commit.ops.len()];
            commits.received(result(seq, applied)).unwrap();
            assert!(commits.acknowledge(&mut trees, seq));
            trees.settle(commits.aliases());
        }
        effects_for(&mut trees, &mut ids, "n");
    }
    assert!(sent.contains(&update("n", 9)), "{sent:?}");
}

#[test]
fn an_agreed_position_another_nodes_stale_s_entry_holds_waits_for_it() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        synced("m", file(ROOT, "c", 2)),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_local("m".into(), local(file(ROOT, "d", 3), 2));
    trees.set_remote("m".into(), remote_at(file(ROOT, "d", 4), 3));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 4));
    trees.settle(commits.aliases());
    assert_eq!(trees.synced().live(&"m".into()), Some(&file(ROOT, "c", 2)));
    trees.set_remote("n".into(), remote_at(file(ROOT, "x", 1), 5));
    trees.settle(commits.aliases());
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle(commits.aliases());
    let commit = build(&mut commits, &mut trees, &mut ids);
    let sent: Vec<op::Op> = commit.iter().flat_map(ops).collect();
    assert!(
        !sent
            .iter()
            .any(|op| matches!(op, op::Op::Move(moved) if moved.node_id == "n")),
        "{sent:?}"
    );
    assert_eq!(trees.synced().live(&"n".into()), Some(&file(ROOT, "c", 1)));
    assert_eq!(trees.class(&"n".into()), Class::Remote);
}

#[test]
fn agreed_positions_that_fit_together_are_taken_when_another_agreement_is_refused() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "p", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "m",
            Some(file(ROOT, "q", 2)),
            Some(file(ROOT, "b", 2)),
            Some(remote(file(ROOT, "b", 2))),
        ),
        (
            "k",
            Some(file(ROOT, "r", 3)),
            Some(file(ROOT, "e", 3)),
            Some(remote(file(ROOT, "e", 3))),
        ),
        synced("h", file(ROOT, "g", 4)),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let mut sent = ops(&commit);
    sent.sort_by_key(|op| format!("{op:?}"));
    let mut expected = [mv("k", ROOT, "r"), mv("m", ROOT, "q"), mv("n", ROOT, "p")];
    expected.sort_by_key(|op| format!("{op:?}"));
    assert_eq!(sent, expected);
    trees.set_local("h".into(), local(file(ROOT, "h2", 5), 4));
    trees.set_remote("h".into(), remote_at(file(ROOT, "h2", 6), 3));
    trees.set_local("n".into(), local(file(ROOT, "q", 1), 1));
    trees.set_local("m".into(), local(file(ROOT, "p", 2), 2));
    trees.set_local("k".into(), local(file(ROOT, "g", 3), 3));
    trees.set_remote("n".into(), remote_at(file(ROOT, "q", 1), 4));
    trees.set_remote("m".into(), remote_at(file(ROOT, "p", 2), 4));
    trees.set_remote("k".into(), remote_at(file(ROOT, "g", 3), 4));
    trees.settle(commits.aliases());
    assert_eq!(trees.synced().live(&"h".into()), Some(&file(ROOT, "g", 4)));
    trees.set_remote("n".into(), remote_at(file(ROOT, "x", 1), 5));
    trees.settle(commits.aliases());
    let applied = vec![outcome(op_result::Code::Applied); 3];
    commits.received(result(2, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    assert_eq!(trees.synced().live(&"n".into()), Some(&file(ROOT, "q", 1)));
    assert_eq!(trees.synced().live(&"m".into()), Some(&file(ROOT, "p", 2)));
    trees.settle(commits.aliases());
    let commit = build(&mut commits, &mut trees, &mut ids);
    let sent: Vec<op::Op> = commit.iter().flat_map(ops).collect();
    assert!(
        !sent
            .iter()
            .any(|op| matches!(op, op::Op::Move(moved) if moved.node_id == "n")),
        "{sent:?}"
    );
    assert_eq!(trees.class(&"n".into()), Class::Remote);
    assert_eq!(trees.synced().live(&"k".into()), Some(&file(ROOT, "g", 3)));
}

#[test]
fn a_late_acknowledgement_keeps_a_position_r_took_again_after_the_commit() {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 2));
    trees.settle(commits.aliases());
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 3));
    trees.settle(commits.aliases());
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 4));
    trees.settle(commits.aliases());
    assert_eq!(trees.synced().live(&"n".into()), Some(&file(ROOT, "c", 1)));
    trees.set_local("n".into(), local(file(ROOT, "b", 1), 1));
    commits
        .received(result(3, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle(commits.aliases());
    assert_eq!(trees.synced().live(&"n".into()), Some(&file(ROOT, "c", 1)));
    trees.set_remote("n".into(), remote_at(file(ROOT, "d", 1), 5));
    trees.settle(commits.aliases());
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live(&"n".into()), Some(&file(ROOT, "b", 1)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
}

#[test]
fn a_late_acknowledgement_takes_the_sent_position_over_one_s_took_from_before_the_commit() {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 2));
    trees.settle(commits.aliases());
    assert_eq!(trees.synced().live(&"n".into()), Some(&file(ROOT, "c", 1)));
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 3));
    commits
        .received(result(3, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle(commits.aliases());
    assert_eq!(trees.synced().live(&"n".into()), Some(&file(ROOT, "b", 1)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live(&"n".into()), Some(&file(ROOT, "c", 1)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "c")]);
}

#[test]
fn a_late_acknowledgement_takes_the_sent_position_when_only_r_returned_to_the_one_s_holds() {
    a_late_acknowledgement_takes_the_sent_position_when_only_r_returned(&[]);
}

#[test]
fn a_late_acknowledgement_takes_the_sent_position_when_r_returned_through_another_position() {
    a_late_acknowledgement_takes_the_sent_position_when_only_r_returned(&["x"]);
}
