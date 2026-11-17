// SPDX-License-Identifier: Apache-2.0

#[test]
fn an_edit_that_became_a_conflict_copy_is_asked_about_with_both_files() {
    use crate::question::QuestionKind;

    let mut remote = Remote::new();
    let ((site_a, _), (site_b, _)) = edited_on_both(&mut remote);

    let both = [(COPY, "from b"), ("main.go", "from a")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), both);
    assert_eq!(files(&site_b.root), both);
    assert_eq!(shown(&site_a), []);
    let [question] = &shown(&site_b)[..] else {
        panic!("one question: {:?}", shown(&site_b));
    };
    let nodes = server_nodes(&remote);
    let QuestionKind::Conflict { original, copy, .. } = &question.kind else {
        panic!("a conflict: {question:?}");
    };
    assert_eq!(original.node_id, live_id(&nodes, "main.go"));
    assert_eq!(copy.node_id, live_id(&nodes, COPY));
    let modified_at_ms = |node_id: &str| {
        let current = remote.space.node(node_id).unwrap().current.unwrap();
        let mtime_ns = current.content.unwrap().mtime_ns;
        assert!(mtime_ns > 0, "{node_id} has a modification time");
        Some(mtime_ns / 1_000_000)
    };
    let items: Vec<_> = (question.items.iter())
        .map(|item| {
            (
                item.path.unquoted(),
                item.node_id.as_str(),
                item.device_id.as_deref(),
                item.size,
                item.modified_at_ms,
            )
        })
        .collect();
    assert_eq!(
        items,
        [
            (
                COPY,
                copy.node_id.as_str(),
                Some("b"),
                Some(6),
                modified_at_ms(&copy.node_id),
            ),
            (
                "main.go",
                original.node_id.as_str(),
                Some("a"),
                Some(6),
                modified_at_ms(&original.node_id),
            ),
        ]
    );
}

/// Keep both closes the question and changes no file.
#[test]
fn keeping_both_files_of_a_conflict_changes_nothing() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    answer(&site_b, Answer::KeepBoth);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    assert_eq!(shown(&site_b), []);

    let both = [(COPY, "from b"), ("main.go", "from a")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), both);
    assert_eq!(files(&site_b.root), both);
    assert_eq!(remote.names(), [COPY, "main.go"]);
}

/// Keep the original moves the conflict copy to the server's trash, and every device removes it.
#[test]
fn keeping_the_original_of_a_conflict_moves_the_copy_to_the_trash() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    let before = server_nodes(&remote);
    answer(&site_b, Answer::KeepOriginal);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    let kept = [("main.go".to_owned(), "from a".to_owned())];
    assert_eq!(files(&site_a.root), kept);
    assert_eq!(files(&site_b.root), kept);
    let after = server_nodes(&remote);
    assert_eq!(live_id(&after, "main.go"), live_id(&before, "main.go"));
    assert!(
        after.contains(&(COPY.to_owned(), live_id(&before, COPY), false)),
        "{after:?}"
    );
    assert_eq!(shown(&site_b), []);
}

/// Keep the copy gives the original the copy's content, so the original's own content stays in its version history,
/// and then moves the copy to the server's trash. Each step is a commit of one op.
#[test]
fn keeping_the_copy_of_a_conflict_gives_the_original_its_content_and_moves_the_copy_to_the_trash() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    let before = server_nodes(&remote);
    answer(&site_b, Answer::KeepCopy);
    let step = b.folder.requested();
    b.take(step).unwrap();
    let sent = b.sent.len();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    let kept = [("main.go".to_owned(), "from b".to_owned())];
    assert_eq!(files(&site_a.root), kept);
    assert_eq!(files(&site_b.root), kept);
    let answers: Vec<_> = b.sent[sent..].iter().map(kinds).collect();
    assert_eq!(answers, [["update content"], ["delete"]]);
    let after = server_nodes(&remote);
    let original = live_id(&before, "main.go");
    assert_eq!(live_id(&after, "main.go"), original);
    assert!(
        after.contains(&(COPY.to_owned(), live_id(&before, COPY), false)),
        "{after:?}"
    );
    let request = ListVersions {
        node_id: original,
        ..Default::default()
    };
    let versions = remote.space.list_versions(&request).unwrap().versions;
    let writers: Vec<_> = (versions.iter())
        .map(|version| version.device_id.as_str())
        .collect();
    assert_eq!(writers, ["b", "a", "a"]);
    assert_eq!(shown(&site_b), []);
}

#[test]
fn a_kept_original_the_server_lost_moves_the_copy_to_the_trash_again() {
    let (site, restored) = answered_then_lost(Answer::KeepOriginal);
    let kept = [("main.go".to_owned(), "from a".to_owned())];
    assert_eq!(files(&site.root), kept);
    assert_eq!(restored.names(), ["main.go"]);
    assert_eq!(shown(&site), []);
}

#[test]
fn a_kept_copy_the_server_lost_gives_the_original_its_content_again() {
    let (site, mut restored) = answered_then_lost(Answer::KeepCopy);
    let kept = [("main.go".to_owned(), "from b".to_owned())];
    assert_eq!(files(&site.root), kept);
    assert_eq!(restored.names(), ["main.go"]);
    let (other, mut c) = linked(&mut restored, "c");
    c.settle();
    assert_eq!(files(&other.root), kept);
    assert_eq!(shown(&site), []);
}

/// Another device edits the copy before the server takes the answer to delete it. The server keeps the copy, and
/// the conflict is asked again.
#[test]
fn a_conflict_whose_file_changes_before_the_answer_reaches_the_server_is_asked_again() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    let [first] = &shown(&site_b)[..] else {
        panic!("one question");
    };
    answer(&site_b, Answer::KeepOriginal);
    let step = b.folder.requested();
    b.take(step).unwrap();
    assert!(matches!(b.work.front(), Some(Work::Send(_))));
    fs::write(site_a.root.join(COPY), b"copy edited after the answer").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.run(&mut remote);
    let result = &remote.results[&b.sent.last().unwrap().commit_id];
    assert_eq!(
        result.results[0].code,
        koidrive_proto::v1::op_result::Code::Superseded as i32
    );
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    assert_eq!(remote.names(), [COPY, "main.go"]);
    assert_eq!(
        fs::read(site_b.root.join(COPY)).unwrap(),
        b"copy edited after the answer"
    );
    let [second] = &shown(&site_b)[..] else {
        panic!("one question: {:?}", shown(&site_b));
    };
    assert_ne!(second.id, first.id);
}

#[test]
fn a_kept_original_deleted_before_the_answer_reaches_the_server_keeps_the_copy() {
    let (remote, site_a, site_b) = original_changed_before_the_answer_reaches_the_server(|root| {
        fs::remove_file(root.join("main.go")).unwrap()
    });

    assert_eq!(remote.names(), [COPY]);
    let copy = [(COPY.to_owned(), "from b".to_owned())];
    assert_eq!(files(&site_a.root), copy);
    assert_eq!(files(&site_b.root), copy);
}

#[test]
fn a_kept_original_renamed_before_the_answer_reaches_the_server_keeps_the_copy() {
    let (remote, site_a, site_b) = original_changed_before_the_answer_reaches_the_server(|root| {
        fs::rename(root.join("main.go"), root.join("renamed.go")).unwrap()
    });

    assert_eq!(remote.names(), [COPY, "renamed.go"]);
    let both = [(COPY, "from b"), ("renamed.go", "from a")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), both);
    assert_eq!(files(&site_b.root), both);
}

/// The server rejects the op that carries out keep the original, and neither file changed. The engine drops the
/// answer, so the folder shows the conflict again under the same ID with no answer and marks the questions changed.
#[test]
fn a_conflict_answer_the_server_rejects_is_asked_again_under_the_same_id() {
    let mut remote = Remote::new();
    let (_, (site_b, mut b)) = edited_on_both(&mut remote);
    let [first] = &shown(&site_b)[..] else {
        panic!("one question");
    };
    answer(&site_b, Answer::KeepOriginal);
    let step = b.folder.requested();
    b.take(step).unwrap();
    assert_eq!(b.folder.view().questions, 0);
    let Some(Work::Send(commit)) = b.work.pop_front() else {
        panic!("the answer is sent");
    };
    assert_eq!(kinds(&commit), ["delete"]);
    b.folder.take_questions_changed();
    let last_seq = b.folder.engine.cursor.as_ref().unwrap().last_seq;
    let result = CommitResult {
        seq: last_seq + 1,
        results: vec![koidrive_proto::v1::OpResult {
            code: op_result::Code::Rejected as i32,
            reject_reason: "INVALID_ARGUMENT".into(),
            ..Default::default()
        }],
        changes: Vec::new(),
        ..Default::default()
    };
    let step = b.folder.sent(Ok(result));
    b.take(step).unwrap();

    assert!(b.folder.take_questions_changed());
    assert_eq!(b.folder.view().questions, 1);
    let [again] = &shown(&site_b)[..] else {
        panic!("one question: {:?}", shown(&site_b));
    };
    assert_eq!(again.id, first.id);
}

/// A conflict in a folder that another device renames shows both files at their new paths, under the same id.
#[test]
fn a_conflict_shows_its_files_at_their_new_paths_after_their_folder_is_renamed() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("docs")).unwrap();
    a.create(&mut remote, &site_a.root.join("docs/main.go"), b"v1");
    let (site_b, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::write(site_a.root.join("docs/main.go"), b"from a").unwrap();
    fs::write(site_b.root.join("docs/main.go"), b"from b").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    let [first] = &shown(&site_b)[..] else {
        panic!("one question: {:?}", shown(&site_b));
    };
    let paths = |question: &crate::question::Question| {
        let mut paths: Vec<String> = (question.items.iter())
            .map(|item| item.path.clone().into())
            .collect();
        paths.sort();
        paths
    };
    assert_eq!(
        paths(first),
        [format!("docs/{COPY}"), "docs/main.go".to_owned()]
    );

    fs::rename(site_a.root.join("docs"), site_a.root.join("papers")).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    let [again] = &shown(&site_b)[..] else {
        panic!("one question: {:?}", shown(&site_b));
    };
    assert_eq!(again.id, first.id);
    assert_eq!(
        paths(again),
        [format!("papers/{COPY}"), "papers/main.go".to_owned()]
    );
}

/// Device `a` edits the original, and its observer has not seen the edit when `b`'s answer to keep the copy
/// reaches it. The edit is kept in a file the person sees, on both devices, and both come to rest.
#[test]
fn an_edit_of_the_original_on_another_device_is_kept_when_the_copy_is_kept() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    answer(&site_b, Answer::KeepCopy);
    let step = b.folder.requested();
    b.take(step).unwrap();
    b.run(&mut remote);
    fs::write(site_a.root.join("main.go"), b"edited on a").unwrap();
    a.pull(&remote);
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    for site in [&site_a, &site_b] {
        let files = files(&site.root);
        assert!(
            files.iter().any(|(_, text)| text == "edited on a"),
            "{files:?}"
        );
        assert!(files.iter().any(|(_, text)| text == "from b"), "{files:?}");
    }
}

/// Renaming the copy by hand, here on the other device, ends the conflict.
#[test]
fn a_conflict_ends_when_someone_renames_a_file_by_hand() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    fs::rename(site_a.root.join(COPY), site_a.root.join("other.go")).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    assert_eq!(shown(&site_b), []);
    assert_eq!(remote.names(), ["main.go", "other.go"]);
}

/// Deleting the copy by hand on the device that asks ends the conflict.
#[test]
fn a_conflict_ends_when_the_person_deletes_a_file_by_hand() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    fs::remove_file(site_b.root.join(COPY)).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    assert_eq!(shown(&site_b), []);
    assert_eq!(remote.names(), ["main.go"]);
}

/// An edit of either file asks the question again under a new ID. The answer to the new question applies to the
/// files as they are now.
#[test]
fn a_conflict_whose_file_changes_is_asked_again() {
    use crate::question::QuestionKind;

    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    let [first] = &shown(&site_b)[..] else {
        panic!("one question");
    };
    fs::write(site_a.root.join("main.go"), b"from a again").unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    let [second] = &shown(&site_b)[..] else {
        panic!("one question: {:?}", shown(&site_b));
    };
    assert_ne!(second.id, first.id);
    let revisions = |question: &crate::question::Question| match &question.kind {
        QuestionKind::Conflict { original, .. } => original.revisions,
        kind => panic!("a conflict: {kind:?}"),
    };
    assert!(revisions(second).content_rev > revisions(first).content_rev);
    answer(&site_b, Answer::KeepOriginal);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    assert_eq!(
        files(&site_b.root),
        [("main.go".to_owned(), "from a again".to_owned())]
    );
}

/// The question outlives the process, and its answer is carried out by the next one.
#[test]
fn a_conflict_is_asked_again_after_a_restart_and_answered_then() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, b)) = edited_on_both(&mut remote);
    let before = shown(&site_b);
    drop(b);
    let mut b = site_b.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert_eq!(b.folder.view().questions, 1);
    assert_eq!(shown(&site_b), before);
    answer(&site_b, Answer::KeepCopy);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    assert_eq!(
        files(&site_a.root),
        [("main.go".to_owned(), "from b".to_owned())]
    );
}

/// Two devices create a file of one name. The second commit gets a conflict name from the server, and its device
/// asks about the pair.
#[test]
fn a_file_created_under_a_name_another_device_took_is_asked_about() {
    use crate::question::QuestionKind;

    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"from a");
    b.create(&mut remote, &site_b.root.join("notes.txt"), b"from b");
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    let copy = "notes (conflict from b 2023-11-14).txt";
    assert_eq!(remote.names(), [copy, "notes.txt"]);
    let nodes = server_nodes(&remote);
    let [question] = &shown(&site_b)[..] else {
        panic!("one question: {:?}", shown(&site_b));
    };
    let QuestionKind::Conflict {
        original,
        copy: renamed,
        ..
    } = &question.kind
    else {
        panic!("a conflict: {question:?}");
    };
    assert_eq!(original.node_id, live_id(&nodes, "notes.txt"));
    assert_eq!(renamed.node_id, live_id(&nodes, copy));
    assert_eq!(shown(&site_a), []);
}

/// The person edits the copy on `b`, and the observer has not seen it when the answer to delete the copy is
/// carried out. The edit is kept on both devices.
#[test]
fn an_edit_of_the_copy_the_answer_would_delete_is_kept() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    fs::write(site_b.root.join(COPY), b"edited copy").unwrap();
    answer(&site_b, Answer::KeepOriginal);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    assert!(b.sent.iter().any(|commit| kinds(commit) == ["delete"]));
    for site in [&site_a, &site_b] {
        let files = files(&site.root);
        assert!(
            files.iter().any(|(_, text)| text == "edited copy"),
            "{files:?}"
        );
        assert!(
            files.contains(&("main.go".to_owned(), "from a".to_owned())),
            "{files:?}"
        );
    }
}

/// The person edits the original, and the upload of that edit is under way when they answer keep the copy. The
/// edit is not taken for the answer: it stays, and the conflict is asked again about the new content.
#[test]
fn an_edit_of_the_original_sent_before_the_answer_asks_again() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    let [first] = &shown(&site_b)[..] else {
        panic!("one question");
    };
    fs::write(site_b.root.join("main.go"), b"edited before the answer").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    assert!(matches!(b.work.front(), Some(Work::Upload(_))));
    answer(&site_b, Answer::KeepCopy);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    assert_eq!(remote.names(), [COPY, "main.go"]);
    assert_eq!(
        fs::read(site_b.root.join("main.go")).unwrap(),
        b"edited before the answer"
    );
    let [second] = &shown(&site_b)[..] else {
        panic!("one question: {:?}", shown(&site_b));
    };
    assert_ne!(second.id, first.id);
}

/// Two conflict copies share one original. Keeping the first copy changes the original, so the other conflict is
/// asked again under a new ID instead of taking an answer meant for the original as it was.
#[test]
fn keeping_one_of_two_copies_of_an_original_asks_about_the_other_again() {
    use crate::question::QuestionKind;

    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    let first_copy = live_id(&server_nodes(&remote), COPY);
    fs::write(site_a.root.join("main.go"), b"second from a").unwrap();
    fs::write(site_b.root.join("main.go"), b"second from b").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    let questions = shown(&site_b);
    assert_eq!(questions.len(), 2, "{questions:?}");
    let is_first = |question: &crate::question::Question| matches!(&question.kind, QuestionKind::Conflict { copy, .. } if copy.node_id == first_copy);
    let chosen = questions
        .iter()
        .find(|question| is_first(question))
        .unwrap();
    let other = questions
        .iter()
        .find(|question| !is_first(question))
        .unwrap();
    let mut store = Store::open(&site_b.db).unwrap();
    let given = koidrive_store::GivenAnswer {
        answer: Answer::KeepCopy.as_str().to_owned(),
        answered_at_ms: NOW,
    };
    store
        .transaction(|tx| tx.answer_question(&chosen.id, &given))
        .unwrap();
    drop(store);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    let [remaining] = &shown(&site_b)[..] else {
        panic!("one question: {:?}", shown(&site_b));
    };
    assert_ne!(remaining.id, other.id);
    assert!(!is_first(remaining));
}

/// Another device edits the copy while the commit that gives the original the copy's content is in flight. The
/// applied answer stays applied: the original keeps the content it was given, the edited copy stays, and the
/// conflict is asked again.
#[test]
fn an_edit_of_the_copy_while_the_original_takes_its_content_is_asked_about_again() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    answer(&site_b, Answer::KeepCopy);
    let step = b.folder.requested();
    b.take(step).unwrap();
    let Some(upload @ Work::Upload(_)) = b.work.pop_front() else {
        panic!("the answer's content is checked first");
    };
    b.perform(upload, &mut remote).unwrap();
    assert!(matches!(b.work.front(), Some(Work::Send(_))));
    fs::write(site_a.root.join(COPY), b"copy edited meanwhile").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.run(&mut remote);
    let result = &remote.results[&b.sent.last().unwrap().commit_id];
    assert_eq!(
        result.results[0].code,
        koidrive_proto::v1::op_result::Code::Applied as i32
    );
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    let kept = [(COPY, "copy edited meanwhile"), ("main.go", "from b")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), kept);
    assert_eq!(files(&site_b.root), kept);
    assert_eq!(shown(&site_b).len(), 1);
}

#[test]
fn a_lost_keep_original_is_not_sent_again_once_another_device_deleted_the_original() {
    let (restored, site_b, resent) = keep_original_lost_by_a_restore(true);

    assert_eq!(restored.names(), [COPY]);
    assert_eq!(
        files(&site_b.root),
        [(COPY.to_owned(), "from b".to_owned())]
    );
    assert_eq!(
        resent_deletes(&resent),
        Vec::<&koidrive_proto::v1::Delete>::new()
    );
}

#[test]
fn a_lost_keep_original_is_sent_again_requiring_the_original() {
    let (restored, site_b, resent) = keep_original_lost_by_a_restore(false);

    assert_eq!(restored.names(), ["main.go"]);
    assert_eq!(
        files(&site_b.root),
        [("main.go".to_owned(), "from a".to_owned())]
    );
    let original = (restored.space.nodes().into_iter())
        .find(|node| node.name == "main.go")
        .unwrap();
    let required: Vec<_> = (resent_deletes(&resent).into_iter())
        .map(|delete| delete.requires.clone().map(|kept| kept.node_id))
        .collect();
    assert_eq!(required, [Some(original.node_id)]);
}

#[test]
fn a_lost_keep_original_below_a_removed_folder_keeps_the_copy_once_the_original_is_deleted() {
    let (restored, sent) = keep_original_lost_below_a_removed_folder(true);

    assert_eq!(restored.names(), ["d", COPY]);
    let deleted: Vec<_> = (resent_deletes(std::slice::from_ref(&sent)).into_iter())
        .map(|delete| delete.requires.is_some())
        .collect();
    assert_eq!(deleted, [true]);
}

#[test]
fn a_lost_keep_original_below_a_removed_folder_deletes_the_folder_after_the_copy() {
    let (restored, _) = keep_original_lost_below_a_removed_folder(false);

    assert_eq!(restored.names(), ["main.go"]);
}

#[test]
fn a_lost_keep_original_whose_folder_the_person_removed_with_the_original_brings_nothing_back() {
    let (restored, site_b) = keep_original_lost_then_its_folder_removed(false);

    assert_eq!(restored.names(), Vec::<String>::new());
    assert_eq!(names(&site_b.root), Vec::<String>::new());
}

#[test]
fn a_lost_keep_original_moved_out_of_a_folder_the_person_removed_keeps_only_the_original() {
    let (restored, site_b) = keep_original_lost_then_its_folder_removed(true);

    assert_eq!(restored.names(), ["main.go"]);
    assert_eq!(
        files(&site_b.root),
        [("main.go".to_owned(), "from a".to_owned())]
    );
}

#[test]
fn a_lost_keep_original_whose_original_both_devices_deleted_brings_nothing_back() {
    let (restored, site_b) = keep_original_lost_after_the_original_was_deleted(true);

    assert_eq!(restored.names(), Vec::<String>::new());
    assert_eq!(names(&site_b.root), Vec::<String>::new());
}

#[test]
fn a_lost_keep_original_whose_original_only_other_devices_deleted_keeps_the_copy() {
    let (restored, site_b) = keep_original_lost_after_the_original_was_deleted(false);

    assert_eq!(restored.names(), [COPY]);
    assert_eq!(names(&site_b.root), [COPY]);
}
