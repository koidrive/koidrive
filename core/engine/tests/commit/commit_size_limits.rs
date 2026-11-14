// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_pending_name_stays_within_the_name_limit() {
    let pending = |name: &str, taken: Option<&str>| {
        let mut rows = vec![
            (
                "h",
                Some(dir("n", name)),
                Some(dir(ROOT, name)),
                Some(remote(dir(ROOT, name))),
            ),
            ("n", Some(dir(ROOT, name)), None, None),
        ];
        rows.extend(taken.map(|taken| synced("p", dir(ROOT, taken))));
        let mut trees = trees(&rows);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        let op::Op::CreateDir(create) = &ops(&commit)[0] else {
            panic!("{:?}", ops(&commit));
        };
        normalize_name(&create.name).expect("a valid name")
    };
    for name in ["a".repeat(255), "é".repeat(127), "e\u{301}".repeat(85)] {
        let first = pending(&name, None);
        let second = pending(&name, Some(&first));
        for (pending, suffix) in [(&first, " (pending)"), (&second, " (pending 2)")] {
            let stem = pending.strip_suffix(suffix).expect(suffix);
            assert!(nfc(&name).starts_with(stem), "{pending}");
            assert!(pending.len() <= MAX_NAME_BYTES, "{pending}");
            assert!(pending.len() + "é".len() > MAX_NAME_BYTES || stem == nfc(&name));
        }
    }
}

#[test]
fn a_commit_holds_at_most_the_op_limit_with_parents_first() {
    let mut rows = vec![("d".to_string(), dir(ROOT, "d"))];
    rows.extend((0..MAX_OPS).map(|i| (format!("f{i:04}"), file("d", &format!("{i}"), 1))));
    let mut trees = Trees::new(ROOT);
    for (i, (id, shape)) in rows.into_iter().enumerate() {
        trees.set_local(id, local(shape, i as u128 + 1));
    }
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), MAX_OPS);
    assert!(matches!(ops(&commit)[0], op::Op::CreateDir(_)));
}

#[test]
fn finishes_sent_on_their_own_stop_before_their_change_records_outgrow_one_message() {
    let marked = ["f0", "f1", "f2", "f3", "f4", "f5", "f6", "f7"];
    let holders = ["y0", "y1", "y2", "y3", "y4", "y5", "y6", "y7"];
    let mut rows: Vec<Row> = Vec::new();
    for (i, (id, holder)) in marked.into_iter().zip(holders).enumerate() {
        let (pending, taken) = (format!("x{i} (pending)"), format!("x{i}"));
        let held = large(ROOT, &pending);
        rows.push((id, Some(held.clone()), Some(held.clone()), Some(remote(held))));
        let other = file(ROOT, &taken, 2);
        rows.push((holder, Some(other.clone()), Some(other.clone()), Some(remote(other))));
    }
    let mut trees = trees(&rows);
    for (i, id) in marked.into_iter().enumerate() {
        let node = RemoteNode {
            pending_for: Some(format!("x{i}")),
            pending_by: Some("me".into()),
            ..remote(large(ROOT, &format!("x{i} (pending)")))
        };
        trees.set_remote(id.into(), node);
    }
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), 3, "{commit:?}");
}

#[test]
fn a_commit_stops_before_its_change_records_outgrow_one_message() {
    let mut trees = Trees::new(ROOT);
    for i in 0..8 {
        trees.set_local(format!("f{i}"), local(large(ROOT, &format!("{i}")), i + 1));
    }
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), 3);
}

/// The answer to a conflict keeps the copy, a file of 8,192 chunks, so the original takes the
/// copy's content in an op of about 330 KiB. It counts toward the commit's change records, and
/// the files queued after it wait for the next commit.
#[test]
fn a_conflict_answer_counts_toward_the_change_records_of_its_commit() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "f", 2)),
        Some(file(ROOT, "f", 1)),
        Some(remote(file(ROOT, "f", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let chunk = proto::ChunkRef {
        hash: vec![7; 32],
        size: 4 << 20,
        encoding: 0,
    };
    let copied = Content {
        chunks: vec![chunk; 8192],
        chunk_count: 8192,
        ..content(2)
    };
    let copy = Shape {
        content: Some(copied.clone()),
        ..file(ROOT, "f (conflict)", 2)
    };
    trees.set_remote("f".into(), remote(file(ROOT, "f", 3)));
    trees.set_remote("y".into(), remote(copy.clone()));
    let made = OpResult {
        node_id: "y".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    acknowledge(&mut commits, &mut trees, vec![made]);
    for (id, shape, ino) in [("f", file(ROOT, "f", 3), 1), ("y", copy, 2)] {
        trees.set_local(id.into(), local(shape.clone(), ino));
        trees.set_synced(id.into(), Some(shape));
    }
    for i in 0..4 {
        trees.set_local(format!("g{i}"), local(large(ROOT, &format!("{i}")), i + 3));
    }
    trees.settle();
    let question = commits.questions()[0].id.clone();
    assert!(commits.decide(&question, Answer::KeepCopy));

    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(commit.encoded_len() <= MAX_COMMIT_BYTES);
    let answer = op::Op::UpdateContent(proto::UpdateContent {
        node_id: "f".into(),
        content: Some(copied.clone()),
        resend: false,
    });
    let sent = ops(&commit);
    assert!(sent.contains(&answer));
    assert_eq!(sent.len(), 2);

    let mut took = remote(file(ROOT, "f", 3));
    took.shape.content = Some(copied);
    took.content_rev = 2;
    took.edit_rev = 2;
    trees.set_remote("f".into(), took);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    let next = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert_eq!(next.len(), 3);
    assert!(next.iter().all(|op| matches!(op, op::Op::CreateFile(_))));
}

#[test]
fn a_delete_too_large_for_one_commit_deletes_the_folder_contents_first() {
    let mut rows = vec![(
        "d",
        None,
        Some(dir(ROOT, "d")),
        Some(remote(dir(ROOT, "d"))),
    )];
    for id in ["f0", "f1", "f2", "f3", "f4"] {
        rows.push((id, None, Some(large("d", id)), Some(remote(large("d", id)))));
    }
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let ops = ops(&commit);
    assert_eq!(ops.len(), 3);
    assert!(
        ops.iter()
            .all(|op| op != &delete("d") && matches!(op, op::Op::Delete(_)))
    );
}

#[test]
fn a_commit_that_does_not_fit_is_planned_again_in_halves_and_grows_back() {
    let mut trees = Trees::new(ROOT);
    for i in 0..12 {
        trees.set_local(
            format!("f{i:02}"),
            local(file(ROOT, &format!("{i}"), 1), i + 1),
        );
    }
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let mut sizes = vec![build(&mut commits, &mut trees, &mut ids).unwrap().ops.len()];
    for _ in 0..2 {
        assert_eq!(
            commits.failed(&mut trees, &refusal(error::Code::LimitExceeded, 0)),
            Ok(Failure::Split)
        );
        assert_eq!(commits.in_flight(), None);
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(commit.device_seq, 1);
        sizes.push(commit.ops.len());
    }
    for _ in 0..2 {
        let applied = vec![outcome(op_result::Code::Applied); *sizes.last().unwrap()];
        acknowledge(&mut commits, &mut trees, applied);
        sizes.push(build(&mut commits, &mut trees, &mut ids).unwrap().ops.len());
    }
    assert_eq!(sizes, [12, 6, 3, 6, 3]);
}

#[test]
fn a_delete_that_does_not_fit_alone_deletes_the_folder_contents_first() {
    let mut trees = trees(&[
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        (
            "e",
            None,
            Some(file("d", "e", 1)),
            Some(remote(file("d", "e", 1))),
        ),
        ("s", None, Some(dir("d", "s")), Some(remote(dir("d", "s")))),
        (
            "t",
            None,
            Some(file("s", "t", 1)),
            Some(remote(file("s", "t", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&first), [delete("d")]);
    assert_eq!(
        commits.failed(&mut trees, &refusal(error::Code::LimitExceeded, 0)),
        Ok(Failure::Split)
    );
    let mut second = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    second.sort_by_key(|op| format!("{op:?}"));
    assert_eq!(second, [delete("e"), delete("s")]);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    for (id, shape) in [
        ("e", file("d", "e", 1)),
        ("s", dir("d", "s")),
        ("t", file("s", "t", 1)),
    ] {
        assert_eq!(trees.synced().live(id), None, "{id}");
        trees.set_remote(id.into(), trashed(shape));
    }
    let third = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&third), [delete("d")]);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.synced().live("d"), None);
    assert!(commits.problems().is_empty());
}

#[test]
fn an_op_that_does_not_fit_alone_and_cannot_be_split_is_a_problem() {
    let mut trees = trees(&[
        ("a", Some(file(ROOT, "x", 1)), None, None),
        (
            "e",
            None,
            Some(file(ROOT, "e", 1)),
            Some(remote(file(ROOT, "e", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .failed(&mut trees, &refusal(error::Code::LimitExceeded, 0))
        .unwrap();
    for _ in 0..3 {
        let Some(commit) = build(&mut commits, &mut trees, &mut ids) else {
            break;
        };
        assert_eq!(commit.ops.len(), 1);
        commits
            .failed(&mut trees, &refusal(error::Code::LimitExceeded, 0))
            .unwrap();
    }
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
    let problems: Vec<&str> = commits.problems().iter().map(String::as_str).collect();
    assert_eq!(problems, ["a", "e"]);
}
