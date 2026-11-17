#[test]
fn gun665_an_edit_the_server_lost_with_the_delete_after_it_is_sent_again() {
    let (mut commits, mut trees, mut ids) = edited_at_5_then_lost();
    let deleted = RemoteNode {
        meta_rev: 6,
        content_rev: 5,
        edit_rev: 5,
        ..trashed(file(ROOT, "a", 2))
    };
    trees.set_remote("a".into(), deleted);
    trees.settle();
    let edited = trees.edited_here();
    trees.set_remote("a".into(), remote(file(ROOT, "a", 1)));
    trees.forget_lost(
        Vec::new(),
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 2)]);
}

#[test]
fn gun866_a_resend_refused_for_a_node_another_device_renamed_and_sent_again_sends_the_edit_at_that_name() {
    let (mut trees, mut commits, mut ids) = resent_where_another_device_renamed_it(Some(2));
    trees.set_remote("f".into(), remote_at(file(ROOT, "b", 1), 3));
    acknowledge(&mut commits, &mut trees, vec![id_exists()]);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("f", 2)]);
}

#[test]
fn gun698_an_edit_back_to_s_while_an_edit_is_in_flight_is_sent_after_restore() {
    let (mut commits, mut trees, mut ids) = an_edit_in_flight_of_a_file_another_device_moved();
    trees.set_local("a".into(), local(file(ROOT, "b", 2), 1));
    trees.settle();
    let in_flight = commits.edited_in_flight();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    trees.record_dropped_edits(in_flight);
    let (trashed, edited, moved) = (trees.trashed_here(), trees.edited_here(), trees.moved_here());
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records(|seq| commits.lost_here(seq));
    trees.set_listed_remote("a".into(), version(remote(file(ROOT, "a", 1)), "v1"));
    trees.remove_remote("pad");
    trees.forget_lost(trashed, edited, synced, moved, untaken, records, commits.held_through(), commits.accepted());
    trees.settle();
    let sent = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert!(sent.contains(&update("a", 2)), "{sent:?}");
}


#[test]
fn gun689_an_edit_the_server_applied_as_a_no_op_is_sent_again_whether_or_not_its_result_arrived() {
    let same = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        meta_rev: 3,
        ..remote(file(ROOT, "n", 2))
    };
    let (effects, sent) = served_at_5_then_lost_with_or_without_its_result(
        &[synced("n", file(ROOT, "n", 1))],
        |trees| trees.set_local("n".into(), local(file(ROOT, "n", 2), 1)),
        same,
        &[("n", remote(file(ROOT, "n", 1)))],
    );
    assert_eq!(effects, []);
    assert_eq!(sent_for(&sent, "n"), [&update("n", 2)]);
}


fn gun689_no_op_edit_served_then_lost(served: Served) -> Vec<op::Op> {
    let mut trees = trees(&[synced("n", file(ROOT, "n", 1))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "p", 2);
    commit_file(&mut trees, &mut commits, &mut ids, "q", 4);
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let other = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        meta_rev: 3,
        ..remote(file(ROOT, "n", 2))
    };
    trees.set_remote("n".into(), other);
    if let Served::ResultFirst = served {
        commits.received(result(5, vec![outcome(op_result::Code::Applied)])).unwrap();
    }
    assert!(commits.own_record(5, &commit.commit_id));
    commits.acknowledge(&mut trees, 5);
    trees.settle();
    if let Served::RecordOnlyThenRestart = served {
        commits = Commits::from_state(commits.state()).unwrap();
        trees.restore(trees.state()).unwrap();
        trees.rules_scanned();
    }
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    let (trashed, edited, moved) = (trees.trashed_here(), trees.edited_here(), trees.moved_here());
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records(|seq| commits.lost_here(seq));
    trees.set_listed_remote("n".into(), remote(file(ROOT, "n", 1)));
    trees.set_listed_remote("p".into(), RemoteNode { content_rev: 2, edit_rev: 2, meta_rev: 2, ..remote(file(ROOT, "p", 7)) });
    trees.forget_lost(trashed, edited, synced, moved, untaken, records, commits.held_through(), commits.accepted());
    commits.forget_lost_slots();
    trees.settle();
    build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit))
}

#[test]
fn gun689_a_no_op_edit_whose_record_carries_no_node_is_sent_again_as_an_edit() {
    let all: Vec<(Served, Vec<op::Op>)> = [Served::ResultFirst, Served::RecordOnly, Served::RecordOnlyThenRestart]
        .into_iter()
        .map(|served| (served, gun689_no_op_edit_served_then_lost(served)))
        .collect();
    for (served, sent) in &all {
        println!("GUN689 {served:?}: {:?}", sent_for(sent, "n"));
    }
    for (served, sent) in &all {
        assert_eq!(sent_for(sent, "n"), [&update("n", 2)], "{served:?}");
    }
}


fn gun902_a_repeated_scan_report_changes_nothing(mut trees: Trees, label: &str) {
    let ids: Vec<String> = trees.local().iter().map(|(id, _)| id.clone()).collect();
    for id in ids {
        let node = trees.local().get(&id).unwrap().clone();
        let before = format!("{:?}", trees.state());
        trees.set_local_changed(id.clone(), node);
        assert_eq!(format!("{:?}", trees.state()), before, "{label}: {id}");
    }
}

#[test]
fn gun902_reporting_an_unchanged_file_leaves_the_engine_state_unchanged() {
    let (_, trees, _) = edited_at_5_then_lost();
    gun902_a_repeated_scan_report_changes_nothing(trees, "edited_at_5_then_lost");
    let (_, trees, _) = an_edit_in_flight_of_a_file_another_device_moved();
    gun902_a_repeated_scan_report_changes_nothing(trees, "in_flight_moved");
    let (trees, _, _) = resent_where_another_device_renamed_it(Some(2));
    gun902_a_repeated_scan_report_changes_nothing(trees, "resent_renamed");
    let (trees, _, _) = resent_where_another_device_renamed_it(None);
    gun902_a_repeated_scan_report_changes_nothing(trees, "resent_renamed_unedited");
}


#[test]
fn gun835_a_split_commit_keeps_the_edit_that_beats_a_delete() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "A", 2)),
        Some(file(ROOT, "a", 1)),
        Some(RemoteNode { content_rev: 3, edit_rev: 3, ..remote_at(file(ROOT, "a", 3), 4) }),
    )]);
    trees.set_remote("f".into(), RemoteNode { content_rev: 3, edit_rev: 3, meta_rev: 4, meta_edit_rev: 4, ..trashed(file(ROOT, "a", 3)) });
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let whole = build(&mut commits, &mut trees, &mut ids).unwrap();
    println!("GUN835 whole: {:?}", ops(&whole));
    assert_eq!(commits.failed(&mut trees, &refusal(error::Code::LimitExceeded, 0)), Ok(Failure::Split));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    println!("GUN835 first: {:?}", ops(&first));
    if first.ops.len() == 1 {
        trees.set_remote("f".into(), RemoteNode { content_rev: 3, edit_rev: 3, ..remote_at(file(ROOT, "A", 3), 5) });
        acknowledge(&mut commits, &mut trees, vec![outcome(op_result::Code::Applied)]);
        let second = build(&mut commits, &mut trees, &mut ids).map(|c| ops(&c)).unwrap_or_default();
        println!("GUN835 second: {second:?}");
        assert_eq!(second, [update("f", 2)]);
    }
}

