// SPDX-License-Identifier: Apache-2.0

#[derive(Clone, Copy, Debug)]
enum Served {
    ResultFirst,
    RecordOnly,
    RecordOnlyThenRestart,
}

fn served_at_5_then_lost(
    rows: &[Row],
    change: impl FnOnce(&mut Trees),
    applied: RemoteNode,
    listed: &[(&str, RemoteNode)],
    served: Served,
) -> (Vec<Effect>, Vec<op::Op>) {
    let mut trees = trees(rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "p", 2);
    commit_file(&mut trees, &mut commits, &mut ids, "q", 4);
    change(&mut trees);
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    if let Served::ResultFirst = served {
        let results = vec![outcome(op_result::Code::Applied); commit.ops.len()];
        commits.received(result(5, results)).unwrap();
    }
    assert!(commits.own_record(5, &commit.commit_id));
    if commits.edits_in_flight(&commit.commit_id, "n") {
        trees.set_remote_edited_here("n".into(), applied);
    } else {
        trees.set_remote_made_here("n".into(), applied);
    }
    let acknowledged = commits.acknowledge(&mut trees, 5);
    assert_eq!(acknowledged, matches!(served, Served::ResultFirst));
    trees.settle();
    if let Served::RecordOnlyThenRestart = served {
        commits = Commits::from_state(commits.state()).unwrap();
        trees.restore(trees.state()).unwrap();
        trees.rules_scanned();
    }
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    let (trashed, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records();
    let p = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        ..remote(file(ROOT, "p", 7))
    };
    let listed: BTreeMap<String, RemoteNode> = (listed.iter())
        .map(|(id, node)| ((*id).to_owned(), node.clone()))
        .chain([("p".to_owned(), p)])
        .collect();
    let gone: Vec<String> = (trees.remote().iter())
        .map(|(id, _)| id.clone())
        .filter(|id| !listed.contains_key(id))
        .collect();
    for id in gone {
        trees.remove_remote(&id);
    }
    for (id, node) in listed {
        trees.set_listed_remote(id, node);
    }
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
    commits.forget_lost_slots();
    trees.settle();
    let effects = effects_for(&mut trees, &mut ids, "n");
    let sent = build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    (effects, sent)
}

fn served_at_5_then_lost_with_or_without_its_result(
    rows: &[Row],
    change: impl Fn(&mut Trees),
    applied: RemoteNode,
    listed: &[(&str, RemoteNode)],
) -> (Vec<Effect>, Vec<op::Op>) {
    let run = |served| served_at_5_then_lost(rows, &change, applied.clone(), listed, served);
    let reference = run(Served::ResultFirst);
    for served in [Served::RecordOnly, Served::RecordOnlyThenRestart] {
        assert_eq!(run(served), reference, "{served:?} changed the outcome");
    }
    reference
}

fn sent_for<'a>(sent: &'a [op::Op], node: &str) -> Vec<&'a op::Op> {
    let node_of = |op: &op::Op| match op {
        op::Op::CreateDir(op) => op.node_id.clone(),
        op::Op::CreateFile(op) => op.node_id.clone(),
        op::Op::UpdateContent(op) => op.node_id.clone(),
        op::Op::Move(op) => op.node_id.clone(),
        op::Op::Delete(op) => op.node_id.clone(),
        op::Op::Restore(op) => op.node_id.clone(),
    };
    sent.iter().filter(|op| node_of(op) == node).collect()
}

#[test]
fn a_create_the_server_lost_is_sent_again_into_a_later_deleted_folder_whether_or_not_its_result_arrived() {
    let created = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        meta_rev: 5,
        ..remote(file("f", "n", 3))
    };
    let deleted = RemoteNode {
        meta_rev: 3,
        ..trashed(dir(ROOT, "f"))
    };
    let (effects, sent) = served_at_5_then_lost_with_or_without_its_result(
        &[synced("f", dir(ROOT, "f"))],
        |trees| trees.set_local("n".into(), local(file("f", "n", 3), 50)),
        created,
        &[("f", deleted)],
    );
    assert_eq!(effects, []);
    assert!(
        matches!(&sent_for(&sent, "n")[..], [op::Op::CreateFile(create)] if create.content == Some(content(3))),
        "{sent:?}"
    );
}

#[test]
fn an_edit_the_server_lost_is_sent_again_whether_or_not_its_result_arrived() {
    let edited = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        ..remote(file(ROOT, "n", 2))
    };
    let (effects, sent) = served_at_5_then_lost_with_or_without_its_result(
        &[synced("n", file(ROOT, "n", 1))],
        |trees| trees.set_local("n".into(), local(file(ROOT, "n", 2), 1)),
        edited,
        &[("n", remote(file(ROOT, "n", 1)))],
    );
    assert_eq!(effects, []);
    assert_eq!(sent_for(&sent, "n"), [&update("n", 2)]);
}

#[test]
fn an_edit_the_server_lost_is_sent_again_over_a_later_delete_whether_or_not_its_result_arrived() {
    let edited = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        ..remote(file(ROOT, "n", 2))
    };
    let deleted = RemoteNode {
        meta_rev: 3,
        ..trashed(file(ROOT, "n", 1))
    };
    let (effects, sent) = served_at_5_then_lost_with_or_without_its_result(
        &[synced("n", file(ROOT, "n", 1))],
        |trees| trees.set_local("n".into(), local(file(ROOT, "n", 2), 1)),
        edited,
        &[("n", deleted)],
    );
    assert_eq!(effects, []);
    assert_eq!(sent_for(&sent, "n"), [&update("n", 2)]);
}

#[test]
fn a_delete_the_server_lost_is_sent_again_whether_or_not_its_result_arrived() {
    let deleted = RemoteNode {
        meta_rev: 5,
        trash_group: "5:0".into(),
        ..trashed(file(ROOT, "n", 1))
    };
    let (effects, sent) = served_at_5_then_lost_with_or_without_its_result(
        &[synced("n", file(ROOT, "n", 1))],
        |trees| trees.remove_local("n"),
        deleted,
        &[("n", remote(file(ROOT, "n", 1)))],
    );
    assert_eq!(effects, []);
    assert_eq!(sent_for(&sent, "n"), [&delete("n")]);
}

#[test]
fn a_move_the_server_lost_is_sent_again_whether_or_not_its_result_arrived() {
    let (effects, sent) = served_at_5_then_lost_with_or_without_its_result(
        &[synced("n", file(ROOT, "a", 1))],
        |trees| trees.set_local("n".into(), local(file(ROOT, "b", 1), 1)),
        remote_at(file(ROOT, "b", 1), 5),
        &[("n", remote(file(ROOT, "a", 1)))],
    );
    assert_eq!(effects, []);
    assert_eq!(sent_for(&sent, "n"), [&mv("n", ROOT, "b")]);
}

#[test]
fn a_lost_own_move_the_server_holds_again_as_l_and_s_do_is_no_change() {
    let mut trees = trees(&[synced("n", file(ROOT, "a", 1))]);
    trees.set_remote("n".into(), remote_at(file(ROOT, "a", 1), 5));
    trees.record_moved("n", 5);
    let moved = trees.moved_here();
    assert_eq!(moved.len(), 1);
    trees.set_remote("n".into(), remote_at(file(ROOT, "a", 1), 6));
    trees.forget_lost(vec![], vec![], vec![], moved, vec![], Default::default(), 3, 0);
    trees.settle();
    assert_eq!(trees.class("n"), Class::InSync);
    assert!(trees.state().local_changes.is_empty());
}

#[test]
fn an_edit_in_flight_that_a_reconnect_drops_after_a_loss_is_sent_again_where_another_device_moved_the_file() {
    let (mut commits, mut trees, mut ids) = an_edit_in_flight_of_a_file_another_device_moved();
    let in_flight = commits.edited_in_flight();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    assert!(commits.in_flight().is_none());
    trees.record_dropped_edits(in_flight);
    list_a_at_its_old_place_after_a_loss(&mut trees, &commits);
    let ops = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert!(ops.contains(&update("a", 3)), "{ops:?}");
    assert!(
        !ops.iter().any(|op| matches!(op, op::Op::CreateFile(create) if create.content == Some(content(3)))),
        "{ops:?}"
    );
}

#[test]
fn an_edit_still_in_flight_when_the_listing_after_a_loss_arrives_is_sent_again_where_another_device_moved_the_file() {
    let (mut commits, mut trees, mut ids) = an_edit_in_flight_of_a_file_another_device_moved();
    trees.record_dropped_edits(commits.edited_in_flight());
    list_a_at_its_old_place_after_a_loss(&mut trees, &commits);
    let refused = commits.failed(&refusal(error::Code::AheadOfServer, 0)).unwrap();
    assert_eq!(refused, Failure::Resync);
    let ops = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert!(ops.contains(&update("a", 3)), "{ops:?}");
    assert!(
        !ops.iter().any(|op| matches!(op, op::Op::CreateFile(create) if create.content == Some(content(3)))),
        "{ops:?}"
    );
}

#[test]
fn an_edit_dropped_in_flight_is_sent_again_over_a_delete_made_after_the_restore() {
    let (_, _, built) = after_an_edit_dropped_in_flight(false, deleted_after_the_restore());
    assert!(built.contains(&update("a", 3)), "{built:?}");
}

#[test]
fn an_edit_dropped_in_flight_and_undone_before_the_listing_leaves_a_delete_made_after_the_restore() {
    let (class, effects, built) = after_an_edit_dropped_in_flight(true, deleted_after_the_restore());
    assert_eq!(class, Class::Remote);
    assert!(matches!(&effects[..], [Effect::RemoveFile { .. }]), "{effects:?}");
    assert!(!uploads(&built, 1), "{built:?}");
}

#[test]
fn an_edit_dropped_in_flight_and_undone_before_the_listing_is_not_sent_over_an_edit_made_after_the_restore() {
    let (class, _, built) = after_an_edit_dropped_in_flight(true, edited_after_the_restore());
    assert_eq!(class, Class::Concurrent);
    assert!(
        !built.iter().any(|op| matches!(op, op::Op::UpdateContent(update) if update.node_id == "a")),
        "{built:?}"
    );
}

fn sent_after_two_losses(record_first: bool) -> Vec<op::Op> {
    let shape = dir("f", "n");
    let mut trees = trees(&[synced("f", dir(ROOT, "f")), synced("n", shape.clone())]);
    let records = trees.remote_records();
    trees.remove_remote("n");
    trees.forget_lost(vec![], vec![], vec![], vec![], vec![], records, 0, 0);
    trees.settle();
    assert!(trees.state().resends.contains("n"));
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateDir(create)] if create.node_id == "n"));
    assert!(commits.own_record(8, &commit.commit_id));
    trees.set_remote_made_here("n".into(), RemoteNode { content_rev: 8, edit_rev: 8, ..remote_at(shape, 8) });
    if record_first {
        assert!(!commits.acknowledge(&mut trees, 8));
        trees.settle();
        assert!(!commits.acknowledge(&mut trees, 8));
        trees.settle();
    }
    commits.received(result(8, vec![outcome(op_result::Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let moved = trees.moved_here();
    let taken = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records();
    trees.remove_remote("n");
    trees.set_listed_remote("f".into(), RemoteNode { meta_rev: 2, meta_edit_rev: 2, ..trashed(dir(ROOT, "f")) });
    trees.forget_lost(vec![], vec![], taken, moved, untaken, records, commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle();
    build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit))
}

#[test]
fn a_resend_whose_record_arrives_before_its_result_does_not_recreate_a_deleted_folder_after_a_second_restore() {
    assert_eq!(sent_after_two_losses(false), []);
    assert_eq!(sent_after_two_losses(true), []);
}
