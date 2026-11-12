// SPDX-License-Identifier: Apache-2.0

#[test]
fn results_set_s_to_what_was_sent() {
    let mut trees = trees(&[
        ("a", Some(file(ROOT, "x", 1)), None, None),
        (
            "m",
            Some(file(ROOT, "moved", 2)),
            Some(file(ROOT, "m", 2)),
            Some(remote(file(ROOT, "m", 2))),
        ),
        (
            "u",
            Some(file(ROOT, "u", 4)),
            Some(file(ROOT, "u", 3)),
            Some(remote(file(ROOT, "u", 3))),
        ),
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    trees.set_remote("m".into(), remote(file(ROOT, "moved", 2)));
    trees.set_remote("u".into(), remote(file(ROOT, "u", 4)));
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    use op_result::Code;
    acknowledge(
        &mut commits,
        &mut trees,
        vec![
            outcome(Code::AppliedRenamed),
            outcome(Code::Applied),
            outcome(Code::Applied),
            outcome(Code::Superseded),
        ],
    );
    assert_eq!(trees.synced().live("a"), Some(&file(ROOT, "x", 1)));
    assert_eq!(
        trees.class("a"),
        Class::Remote,
        "the conflict name reaches the disk"
    );
    assert_eq!(trees.synced().live("d"), None);
    assert_eq!(
        trees.class("d"),
        Class::Remote,
        "the folder the server kept comes back"
    );
    assert_eq!(trees.class("m"), Class::InSync);
    assert_eq!(trees.class("u"), Class::InSync);
    assert_eq!(commits.accepted(), 1);
    assert!(commits.problems().is_empty());
}

#[test]
fn a_folder_merged_into_one_l_holds_elsewhere_is_removed_once_its_files_follow_into_it() {
    let mut trees = trees(&[
        ("a", Some(dir(ROOT, "d")), None, None),
        (
            "b",
            Some(dir(ROOT, "e")),
            Some(dir(ROOT, "e")),
            Some(remote(dir(ROOT, "e"))),
        ),
        ("c", Some(file("a", "f", 1)), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    use op_result::Code;
    let merged = OpResult {
        node_id: "b".into(),
        ..outcome(Code::MergedInto)
    };
    acknowledge(
        &mut commits,
        &mut trees,
        vec![merged, outcome(Code::Applied)],
    );
    trees.set_remote("c".into(), remote(file("b", "f", 1)));
    assert_eq!(trees.synced().live("a"), None);
    assert_eq!(trees.synced().live("c"), Some(&file("b", "f", 1)));
    trees.set_local("g".into(), local(file("a", "g", 2), 9));
    let later = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&later)[..], [op::Op::CreateFile(c)] if c.parent_id == "b"));
    acknowledge(&mut commits, &mut trees, vec![outcome(Code::Applied)]);
    trees.set_remote("g".into(), remote(file("b", "g", 2)));
    let reverted: Vec<&str> = trees.reverted().iter().map(String::as_str).collect();
    assert_eq!(reverted, ["a", "c", "g"]);
    let mut effects = Effects::default();
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        effects
            .done(&mut trees, commits.in_flight(), &record.effect_id, None)
            .unwrap();
        trees.settle();
    }
    assert_eq!(trees.local().get("a"), None);
    assert_eq!(trees.local().get("c").unwrap().shape, file("b", "f", 1));
    assert_eq!(trees.local().get("g").unwrap().shape, file("b", "g", 2));
    let b = trees.local().get("b").unwrap();
    assert_eq!((&b.shape, b.identity.ino), (&dir(ROOT, "e"), 2));
    assert!(trees.reverted().is_empty());
    trees.settle();
    assert!(trees.work().is_empty());
}

#[test]
fn a_conflict_copy_takes_the_local_file_and_the_original_is_downloaded_again() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "f", 2)),
        Some(file(ROOT, "f", 1)),
        Some(remote(file(ROOT, "f", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("f".into(), remote(file(ROOT, "f", 3)));
    trees.set_remote("y".into(), remote(file(ROOT, "f (conflict)", 2)));
    let copy = OpResult {
        node_id: "y".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    acknowledge(&mut commits, &mut trees, vec![copy]);
    assert_eq!(trees.local().get("y").unwrap().identity.ino, 1);
    assert_eq!(trees.synced().live("y"), Some(&file(ROOT, "f", 2)));
    assert_eq!(trees.class("y"), Class::Remote);
    assert_eq!(
        (trees.local().get("f"), trees.synced().live("f")),
        (None, None)
    );
    assert_eq!(trees.class("f"), Class::Remote);
}

#[test]
fn a_conflict_copy_another_device_deleted_before_its_result_is_deleted_here() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "f", 2)),
        Some(file(ROOT, "f", 1)),
        Some(remote(file(ROOT, "f", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("f".into(), remote(file(ROOT, "f", 3)));
    trees.set_remote("y".into(), trashed(file(ROOT, "f (conflict)", 2)));
    let copy = OpResult {
        node_id: "y".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    acknowledge(&mut commits, &mut trees, vec![copy]);
    assert_eq!(trees.class("y"), Class::Remote);
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
}

/// The device's own commit makes the conflict copy y, and its answer is lost. A log replay after
/// the reconnect brings y into R before the stored result arrives, and the effects download y.
/// The result then finds y on disk already: that entry stays y's, f takes the server's content,
/// and nothing is left to upload.
#[test]
fn a_conflict_copy_downloaded_before_its_result_is_kept_once() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "f", 2)),
        Some(file(ROOT, "f", 1)),
        Some(remote(file(ROOT, "f", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("f".into(), remote(file(ROOT, "f", 3)));
    trees.set_remote("y".into(), remote(file(ROOT, "f (conflict)", 2)));
    trees.settle();
    let mut effects = Effects::default();
    let identity = |ino| FileIdentity {
        dev: 1,
        ino,
        birth_ns: Some(0),
    };
    let download = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
        .unwrap();
    assert_eq!(download.node, "y");
    effects
        .done(
            &mut trees,
            commits.in_flight(),
            &download.effect_id,
            Some(identity(9)),
        )
        .unwrap();
    trees.settle();
    let copy = OpResult {
        node_id: "y".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    acknowledge(&mut commits, &mut trees, vec![copy]);
    let mut ino = 10;
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity(ino)),
            )
            .unwrap();
        ino += 1;
        trees.settle();
    }
    let y = trees.local().get("y").unwrap();
    assert_eq!(
        (&y.shape, y.identity.ino),
        (&file(ROOT, "f (conflict)", 2), 9)
    );
    assert_eq!(trees.local().get("f").unwrap().shape, file(ROOT, "f", 3));
    assert_eq!(trees.local().iter().count(), 2);
    trees.settle();
    assert!(trees.work().is_empty());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

/// As above, but the result arrives while y's download is in flight. The result makes the local
/// file y, so L holds y before the download is done. L does not take the download's outcome, and
/// the local file stays y's: it is moved to y's name, and f is downloaded again.
#[test]
fn a_conflict_copy_whose_result_arrives_during_its_download_keeps_the_local_file() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "f", 2)),
        Some(file(ROOT, "f", 1)),
        Some(remote(file(ROOT, "f", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("f".into(), remote(file(ROOT, "f", 3)));
    trees.set_remote("y".into(), remote(file(ROOT, "f (conflict)", 2)));
    trees.settle();
    let mut effects = Effects::default();
    let identity = |ino| FileIdentity {
        dev: 1,
        ino,
        birth_ns: Some(0),
    };
    let download = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
        .unwrap();
    assert_eq!(download.node, "y");
    let copy = OpResult {
        node_id: "y".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    acknowledge(&mut commits, &mut trees, vec![copy]);
    assert_eq!(trees.local().get("y").unwrap().identity.ino, 1);
    assert!(!download.applies(&trees, commits.in_flight()));
    assert_eq!(
        effects.done(
            &mut trees,
            commits.in_flight(),
            &download.effect_id,
            Some(identity(9))
        ),
        Err(EffectError::DoesNotApply(download.effect_id.clone()))
    );
    effects.failed(&mut trees, &download.effect_id).unwrap();
    effects.unblock("y");
    trees.settle();
    let mut ino = 10;
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity(ino)),
            )
            .unwrap();
        ino += 1;
        trees.settle();
    }
    let y = trees.local().get("y").unwrap();
    assert_eq!(
        (&y.shape, y.identity.ino),
        (&file(ROOT, "f (conflict)", 2), 1)
    );
    assert_eq!(trees.local().get("f").unwrap().shape, file(ROOT, "f", 3));
    assert_eq!(trees.local().iter().count(), 2);
    trees.settle();
    assert!(trees.work().is_empty());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

/// On a disk that tells "A" from "a", the user edits the file "A" and creates the folder "a" while
/// another device edits the file and renames it to "b". The commit makes the edit the conflict copy
/// y, whose S update at "A" waits behind the folder: S cannot hold both names. The effects still
/// move y to the server's name, which ends the wait, and the trees come into sync.
#[test]
fn a_conflict_copy_whose_s_update_waits_behind_a_new_folder_is_moved_to_its_name() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "A", 2)),
            Some(file(ROOT, "A", 1)),
            Some(remote(file(ROOT, "A", 1))),
        ),
        ("d", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit).len(), 2);
    trees.set_remote("n".into(), remote(file(ROOT, "b", 3)));
    trees.set_remote("y".into(), remote(file(ROOT, "b (conflict)", 2)));
    trees.set_remote("d".into(), remote(dir(ROOT, "a")));
    let results = commit
        .ops
        .iter()
        .map(|op| match &op.op {
            Some(op::Op::UpdateContent(_)) => OpResult {
                node_id: "y".into(),
                ..outcome(op_result::Code::ConflictCopy)
            },
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    acknowledge(&mut commits, &mut trees, results);
    assert!(trees.waiting().contains_key("y"));
    let mut effects = Effects::default();
    let mut ino = 10;
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        let identity = FileIdentity {
            dev: 1,
            ino,
            birth_ns: Some(0),
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
        ino += 1;
        trees.settle();
        assert!(ino < 20, "the effects do not end");
    }
    assert_eq!(
        trees.local().get("y").unwrap().shape,
        file(ROOT, "b (conflict)", 2)
    );
    assert_eq!(trees.local().get("n").unwrap().shape, file(ROOT, "b", 3));
    assert!(trees.waiting().is_empty());
    trees.settle();
    assert!(trees.work().is_empty());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn keep_the_original_deletes_the_copy_only_while_the_original_is_at_the_revisions_r_holds() {
    let (mut commits, mut trees, mut ids, _) = answered_conflict(Answer::KeepOriginal);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete_copy_requiring(4, 3)]);
}

#[test]
fn keep_the_copy_deletes_the_copy_only_while_the_original_holds_the_content_it_took() {
    let (mut commits, mut trees, mut ids, copy) = answered_conflict(Answer::KeepCopy);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(ops(&commit)[..], [op::Op::UpdateContent(_)]));
    let took = Shape {
        name: "f".into(),
        ..copy
    };
    trees.set_remote(
        "f".into(),
        RemoteNode {
            content_rev: 6,
            ..remote_at(took.clone(), 3)
        },
    );
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.set_local("f".into(), local(took.clone(), 1));
    trees.set_synced("f".into(), Some(took));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete_copy_requiring(6, 3)]);
}

#[test]
fn a_kept_original_is_forgotten_once_another_device_restores_the_copy() {
    let mut trees = kept_original_recorded();
    trees.set_remote("y".into(), remote_at(file(ROOT, "f (conflict)", 2), 7));
    assert_eq!(kept_originals(&trees), []);
}

#[test]
fn a_kept_original_is_forgotten_once_l_holds_the_copy_again() {
    let mut trees = kept_original_recorded();
    trees.set_local("y".into(), local(file(ROOT, "f (conflict)", 2), 2));
    assert_eq!(kept_originals(&trees), []);
}

#[test]
fn a_kept_original_waiting_to_be_sent_again_is_forgotten_once_another_device_trashes_the_copy() {
    let mut trees = trees(&[
        synced("f", file(ROOT, "f", 1)),
        (
            "y",
            None,
            Some(file(ROOT, "y", 2)),
            Some(remote(file(ROOT, "y", 2))),
        ),
    ]);
    let mut state = trees.state();
    state.kept_originals.insert("y".into(), "f".into());
    trees.restore(state).unwrap();
    let in_trash = RemoteNode {
        trashed: true,
        trash_group: "7:0".into(),
        ..remote_at(file(ROOT, "y", 2), 7)
    };
    trees.set_remote("y".into(), in_trash);
    assert_eq!(kept_originals(&trees), []);
}

#[test]
fn a_resent_answer_delete_keeps_its_precondition_when_another_device_removed_the_kept_file_here() {
    let mut trees = trees(&[
        ("f", None, None, Some(remote(file(ROOT, "f", 1)))),
        (
            "y",
            None,
            Some(file(ROOT, "y", 2)),
            Some(remote(file(ROOT, "y", 2))),
        ),
    ]);
    let mut state = trees.state();
    state.kept_originals.insert("y".into(), "f".into());
    trees.restore(state).unwrap();
    trees.set_rules::<&str>(&[], None);
    trees.rules_scanned();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    trees.set_local("f".into(), local(file(ROOT, "f", 1), 1));
    trees.set_synced("f".into(), Some(file(ROOT, "f", 1)));
    trees.settle();
    let held = trees.remote().get("f").unwrap();
    let required = delete_copy_requiring(held.content_rev, held.meta_rev);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [required]);
}

#[test]
fn a_resent_answer_delete_drops_its_precondition_once_a_delete_of_this_device_trashed_the_kept_file()
 {
    let mut trees = trees(&[
        synced("f", file(ROOT, "f", 1)),
        (
            "a",
            None,
            Some(file(ROOT, "a", 2)),
            Some(remote(file(ROOT, "a", 2))),
        ),
    ]);
    let mut state = trees.state();
    state.kept_originals.insert("a".into(), "f".into());
    trees.restore(state).unwrap();
    trees.set_rules::<&str>(&[], None);
    trees.rules_scanned();
    trees.remove_local("f");
    let state = koidrive_engine::state::CommitsState {
        cap: Some(1),
        ..Commits::default().state()
    };
    let (mut commits, mut ids) = (Commits::from_state(state).unwrap(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("f")]);
    let in_trash = RemoteNode {
        trashed: true,
        trash_group: "1:0".into(),
        ..remote_at(file(ROOT, "f", 1), 1)
    };
    trees.set_remote("f".into(), in_trash);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("a")]);
}
