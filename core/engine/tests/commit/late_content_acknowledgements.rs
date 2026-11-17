// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_late_acknowledgement_keeps_the_content_s_took_after_the_commit_was_built() {
    a_late_acknowledgement_keeps_the_content_s_took_since(false, false);
}

#[test]
fn a_late_acknowledgement_after_a_restart_keeps_the_content_s_took_after_the_commit_was_built() {
    a_late_acknowledgement_keeps_the_content_s_took_since(true, false);
}

#[test]
fn a_late_acknowledgement_overtaken_by_the_next_remote_edit_keeps_the_content_s_took() {
    a_late_acknowledgement_keeps_the_content_s_took_since(false, true);
}

#[test]
fn a_late_acknowledgement_overtaken_by_the_next_remote_edit_after_a_restart_keeps_the_content_s_took()
 {
    a_late_acknowledgement_keeps_the_content_s_took_since(true, true);
}

#[test]
fn a_late_acknowledgement_keeps_the_content_s_took_back_to_what_it_held_at_the_build() {
    a_late_acknowledgement_keeps_the_content_s_took_back(false);
}

#[test]
fn a_late_acknowledgement_after_a_restart_keeps_the_content_s_took_back_to_what_it_held_at_the_build()
 {
    a_late_acknowledgement_keeps_the_content_s_took_back(true);
}

#[test]
fn a_late_acknowledgement_takes_what_was_sent_when_the_other_device_reverts_it() {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "a", 1)),
        Some(file(ROOT, "a", 0)),
        Some(remote(file(ROOT, "a", 0))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 1)]);
    trees.set_remote("n".into(), remote(file(ROOT, "a", 1)));
    trees.set_remote("n".into(), remote(file(ROOT, "a", 0)));
    trees.settle();
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 1)));
    let effects = effects_for(&mut trees, &mut ids, "n");
    assert!(
        matches!(&effects[..], [Effect::WriteFile { content: written, .. }] if *written == content(0)),
        "{effects:?}"
    );
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "a", 0)));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn an_edit_undone_while_its_upload_is_out_is_sent_after_the_acknowledgement() {
    let mut trees = trees(&[synced("n", file(ROOT, "a", 1))]);
    trees.set_local_changed("n".into(), local(file(ROOT, "a", 2), 1));
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
    trees.set_local_changed("n".into(), local(file(ROOT, "a", 1), 1));
    trees.set_remote("n".into(), remote(file(ROOT, "a", 2)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert!(trees.state().local_changes.contains("n"));
    let next = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&next), [update("n", 1)]);
}

#[test]
fn an_edit_undone_while_its_upload_is_out_is_sent_after_an_acknowledgement_that_waits() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        synced("n", file(ROOT, "old", 0)),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.set_local_changed("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
    trees.set_local_changed("n".into(), local(file(ROOT, "old", 0), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "x", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    assert_eq!(
        trees.waiting().get("n").map(|waiting| &waiting.shape),
        Some(&Some(file(ROOT, "x", 1)))
    );
    assert!(trees.state().local_changes.contains("n"));
}

#[test]
fn a_rename_returned_to_its_waiting_shape_stays_a_change_when_its_commit_is_lost() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        synced("n", file(ROOT, "old", 0)),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.set_local_changed("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("n".into(), remote(file(ROOT, "x", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    assert_eq!(
        trees.waiting().get("n").map(|waiting| &waiting.shape),
        Some(&Some(file(ROOT, "x", 1)))
    );
    trees.set_local_changed("n".into(), local(file(ROOT, "z", 1), 2));
    trees.set_local_changed("n".into(), local(file(ROOT, "x", 1), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "old", 0)));
    trees.forget_lost(vec![], vec![], vec![], vec![], vec![], Default::default(), &Default::default(), 0, 0);
    assert!(trees.waiting().get("n").is_none());
    assert!(trees.state().local_changes.contains("n"));
}

#[test]
fn a_rename_back_to_s_while_its_acknowledgement_waits_stays_a_change() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        synced("n", file(ROOT, "old", 0)),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.set_local_changed("n".into(), local(file(ROOT, "x", 0), 2));
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x")]);
    trees.set_remote("n".into(), remote(file(ROOT, "x", 0)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(
        trees.waiting().get("n").map(|waiting| &waiting.shape),
        Some(&Some(file(ROOT, "x", 0)))
    );
    trees.set_local_changed("n".into(), local(file(ROOT, "old", 0), 2));
    trees.settle();
    assert!(trees.state().local_changes.contains("n"));
}

#[test]
fn a_rename_back_to_s_is_no_change_once_a_listing_and_a_loss_take_its_acknowledgement_back() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        synced("n", file(ROOT, "old", 0)),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.set_local_changed("n".into(), local(file(ROOT, "x", 0), 2));
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("n".into(), remote(file(ROOT, "x", 0)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.set_local_changed("n".into(), local(file(ROOT, "old", 0), 2));
    trees.set_listed_remote("n".into(), remote(file(ROOT, "old", 0)));
    trees.forget_lost(vec![], vec![], vec![], vec![], vec![], Default::default(), &Default::default(), 0, 0);
    assert!(trees.waiting().get("n").is_none());
    assert_eq!(trees.check(), Ok(()));
    assert!(trees.state().local_changes.is_empty());
}
