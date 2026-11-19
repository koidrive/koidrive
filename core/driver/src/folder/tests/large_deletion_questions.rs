// SPDX-License-Identifier: Apache-2.0

fn let_the_files_age() {
    let window = std::time::Duration::from_millis(koidrive_engine::guard::WINDOW_MS);
    crate::clock::advance_to(crate::clock::instant() + window);
}

#[test]
fn a_removed_folder_that_holds_most_of_the_space_comes_back_when_the_person_restores_it() {
    let mut remote = Remote::new();
    let (site, mut a) = with_a_held_folder(&mut remote);

    let question = answer(&site, Answer::Restore);
    assert_eq!(
        question.kind,
        crate::question::QuestionKind::MassDelete {
            deletions: 1,
            affected_files: 15,
            space_files: 20,
            exceeded: vec![crate::question::Limit::Share],
        }
    );
    let held: Vec<_> = (question.items.iter())
        .map(|item| (item.path.unquoted(), item.files))
        .collect();
    assert_eq!(held, [("d", 15)]);
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);

    assert_eq!(names(&site.root.join("d")).len(), 15);
    assert_eq!(remote.names().len(), 21);
    assert!(
        a.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(a.folder.view().questions, 0);
    let mut store = Store::open(&site.db).unwrap();
    assert_eq!(store.transaction(|tx| tx.questions()).unwrap(), vec![]);
}

#[test]
fn a_removed_folder_that_holds_most_of_the_space_is_deleted_when_the_person_says_so() {
    let mut remote = Remote::new();
    let (site, mut a) = with_a_held_folder(&mut remote);

    answer(&site, Answer::Delete);
    let step = a.folder.requested();
    a.take(step).unwrap();
    assert_eq!(a.run(&mut remote), ["send"]);

    assert_eq!(kinds(a.sent.last().unwrap()), ["delete"]);
    assert_eq!(remote.names().len(), 5);
    assert_eq!(a.folder.view().questions, 0);
}

#[test]
fn dismissing_the_notice_while_the_folder_is_removed_opens_no_new_one() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    b.folder.disk.set_batch(BatchLimit::ONE);
    fs::create_dir(site_a.root.join("d")).unwrap();
    for i in 0..50 {
        fs::write(site_a.root.join(format!("d/{i}.txt")), format!("kept {i}")).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    let_the_files_age();
    assert_eq!(files(&site_b.root).len(), 50);
    fs::remove_dir_all(site_a.root.join("d")).unwrap();
    a.scan_until(|a| a.folder.view().questions == 1);
    answer(&site_a, Answer::Delete);
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    b.pull(&remote);
    while b.folder.view().questions == 0 {
        let work = b.work.pop_front().expect("removals are still queued");
        b.perform(work, &mut remote).unwrap();
    }
    answer(&site_b, Answer::Dismiss);
    assert!(
        !files(&site_b.root).is_empty(),
        "the folder is still being removed"
    );
    let seq = remote.space.head_seq();
    let step = b.folder.requested();
    b.take(step).unwrap();

    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert_eq!(remote.space.head_seq(), seq);
    assert_eq!(files(&site_b.root).len(), 0);
    let mut store = Store::open(&site_b.db).unwrap();
    assert_eq!(store.transaction(|tx| tx.questions()).unwrap(), vec![]);
}

/// GUN-384 review: a bootstrap right after the dismissal leaves R with no record of the trashed folder. The rest of
/// the folder is still the deletion the person dismissed.
#[test]
fn a_dismissed_folder_stays_dismissed_after_a_bootstrap() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    fs::create_dir(site_a.root.join("d")).unwrap();
    for i in 0..50 {
        fs::write(site_a.root.join(format!("d/{i}.txt")), format!("kept {i}")).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    let_the_files_age();
    fs::remove_dir_all(site_a.root.join("d")).unwrap();
    a.scan_until(|a| a.folder.view().questions == 1);
    answer(&site_a, Answer::Delete);
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    b.pull(&remote);
    while b.folder.view().questions == 0 {
        let work = b.work.pop_front().expect("removals are still queued");
        b.perform(work, &mut remote).unwrap();
    }
    answer(&site_b, Answer::Dismiss);
    let seq = remote.space.head_seq();
    let step = b.folder.requested();
    b.take(step).unwrap();

    let truncated = ResyncRequired {
        reason: Reason::LogTruncated as i32,
        ..Default::default()
    };
    b.event(Event::ResyncRequired(truncated)).unwrap();
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert_eq!(remote.space.head_seq(), seq);
    assert_eq!(files(&site_b.root).len(), 0);
    let mut store = Store::open(&site_b.db).unwrap();
    assert_eq!(store.transaction(|tx| tx.questions()).unwrap(), vec![]);
}

/// GUN-384 review: the server truncates its log after device a deletes the folder `d`, so device b reads a tree that
/// does not list the trash, and R holds no record of `d`. The person dismisses the notice while b still removes the
/// folder. The server is then rolled back to a backup taken after the deletion, and b reads the tree again with the
/// trash. R learns that `d` was trashed at a seq it already held, so the rest of the folder is still the deletion
/// the person dismissed.
#[test]
fn a_dismissed_folder_stays_dismissed_after_a_truncated_log_and_a_rollback() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    b.folder.disk.set_batch(BatchLimit::ONE);
    fs::create_dir(site_a.root.join("d")).unwrap();
    for i in 0..50 {
        fs::write(site_a.root.join(format!("d/{i}.txt")), format!("kept {i}")).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    let_the_files_age();
    fs::remove_dir_all(site_a.root.join("d")).unwrap();
    a.scan_until(|a| a.folder.view().questions == 1);
    answer(&site_a, Answer::Delete);
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    let restored = backup(&remote, &a);
    let seq = remote.space.head_seq();
    remote.space.truncate_log(seq);
    b.pull(&remote);
    while b.folder.view().questions == 0 {
        let work = b.work.pop_front().expect("removals are still queued");
        b.perform(work, &mut remote).unwrap();
    }
    assert!(
        !files(&site_b.root).is_empty(),
        "the folder is still being removed"
    );
    answer(&site_b, Answer::Dismiss);
    let step = b.folder.requested();
    b.take(step).unwrap();
    a.create(
        &mut remote,
        &site_a.root.join("unrelated.txt"),
        b"unrelated",
    );
    b.pull(&remote);

    remote = restored;
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert_eq!(remote.space.head_seq(), seq);
    assert_eq!(files(&site_b.root).len(), 0);
    let mut store = Store::open(&site_b.db).unwrap();
    assert_eq!(store.transaction(|tx| tx.questions()).unwrap(), vec![]);
}

/// GUN-384 review: as above without the rollback, and device a then restores the folder and deletes it again while b
/// misses the log a second time. The bootstrap lists the trash because b's guard holds the dismissed deletion, so R
/// learns that `d` was trashed after that deletion, and the rest of the folder opens a new notice.
#[test]
fn a_folder_restored_and_deleted_again_while_the_log_was_truncated_is_noticed() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    b.folder.disk.set_batch(BatchLimit::ONE);
    fs::create_dir(site_a.root.join("d")).unwrap();
    for i in 0..50 {
        fs::write(site_a.root.join(format!("d/{i}.txt")), format!("kept {i}")).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    let_the_files_age();
    let delete = |a: &mut Device, remote: &mut Remote| {
        fs::remove_dir_all(site_a.root.join("d")).unwrap();
        a.scan_until(|a| a.folder.view().questions == 1);
        answer(&site_a, Answer::Delete);
        let step = a.folder.requested();
        a.take(step).unwrap();
        a.run(remote);
    };
    delete(&mut a, &mut remote);
    remote.space.truncate_log(remote.space.head_seq());
    b.pull(&remote);
    while b.folder.view().questions == 0 {
        let work = b.work.pop_front().expect("removals are still queued");
        b.perform(work, &mut remote).unwrap();
    }
    assert!(
        !files(&site_b.root).is_empty(),
        "the folder is still being removed"
    );
    answer(&site_b, Answer::Dismiss);
    let step = b.folder.requested();
    b.take(step).unwrap();
    let (top, _) = (a.folder.engine.trees.remote().iter())
        .find(|(_, node)| node.shape.name == "d")
        .unwrap();
    let intent = RestoreIntent::from_trash(top.clone(), None, None).unwrap();
    let request = ask(&site_a, &intent);
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(ended(&site_a), [(request, Outcome::Restored)]);
    assert_eq!(files(&site_a.root).len(), 50);
    delete(&mut a, &mut remote);
    remote.space.truncate_log(remote.space.head_seq());

    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert_eq!(files(&site_b.root).len(), 0);
    let mut store = Store::open(&site_b.db).unwrap();
    let questions = store.transaction(|tx| tx.questions()).unwrap();
    assert_eq!(questions.len(), 1, "the later deletion is noticed");
}

/// GUN-384 review: while device b's notice is open, device a deletes two more files. The notice keeps its id and its
/// counts grow, so the number of questions stays 1, and the folder still marks the questions changed for the status.
#[test]
fn a_notice_whose_counts_grow_marks_the_questions_changed() {
    use crate::question::QuestionKind;

    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    fs::create_dir(site_a.root.join("d")).unwrap();
    for i in 0..15 {
        fs::write(site_a.root.join(format!("d/{i}.txt")), format!("in d {i}")).unwrap();
    }
    for i in 0..5 {
        fs::write(
            site_a.root.join(format!("{i}.txt")),
            format!("beside d {i}"),
        )
        .unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    let_the_files_age();
    fs::remove_dir_all(site_a.root.join("d")).unwrap();
    a.scan_until(|a| a.folder.view().questions == 1);
    answer(&site_a, Answer::Delete);
    let step = a.folder.requested();
    a.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    assert_eq!(b.folder.view().questions, 1);
    assert!(b.folder.take_questions_changed());

    for i in 0..2 {
        fs::remove_file(site_a.root.join(format!("{i}.txt"))).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    assert_eq!(files(&site_b.root).len(), 3);
    assert_eq!(b.folder.view().questions, 1);
    assert!(b.folder.take_questions_changed());
    match answer(&site_b, Answer::Dismiss).kind {
        QuestionKind::RemoteMassDelete {
            deletions,
            affected_files,
            ..
        } => assert_eq!((deletions, affected_files), (3, 17)),
        kind => panic!("a notice: {kind:?}"),
    }
}

#[test]
fn a_question_asked_again_under_a_new_id_marks_the_questions_changed() {
    let mut remote = Remote::new();
    let (site, mut a) = with_a_held_folder(&mut remote);
    let asked = |site: &Site| -> Vec<String> {
        let mut store = Store::open(&site.db).unwrap();
        let questions = store.transaction(|tx| tx.questions()).unwrap();
        questions.into_iter().map(|q| q.question_id).collect()
    };
    let before = asked(&site);
    a.folder.take_questions_changed();

    for i in 0..2 {
        fs::remove_file(site.root.join(format!("{i}.txt"))).unwrap();
    }
    eventually("the guard asks again", || {
        a.scan(Duration::from_millis(100));
        a.run(&mut remote);
        asked(&site) != before
    });

    let after = asked(&site);
    assert_eq!(after.len(), 1);
    assert_ne!(after, before, "the guard asks again under a new id");
    assert_eq!(a.folder.view().questions, 1);
    assert!(a.folder.take_questions_changed());
}

#[test]
fn another_devices_deletion_of_most_of_the_space_is_noticed_until_it_is_dismissed() {
    use crate::question::{Limit, QuestionKind};

    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    fs::create_dir(site_a.root.join("d")).unwrap();
    for i in 0..15 {
        fs::write(site_a.root.join(format!("d/{i}.txt")), format!("in d {i}")).unwrap();
    }
    for i in 0..5 {
        fs::write(
            site_a.root.join(format!("{i}.txt")),
            format!("beside d {i}"),
        )
        .unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    let_the_files_age();
    assert_eq!(files(&site_b.root).len(), 20);
    fs::remove_dir_all(site_a.root.join("d")).unwrap();
    a.scan_until(|a| a.folder.view().questions == 1);
    answer(&site_a, Answer::Delete);
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    assert_eq!(remote.names().len(), 5);

    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));

    assert_eq!(files(&site_b.root).len(), 5);
    assert_eq!(b.folder.view().questions, 1);
    let notice = answer(&site_b, Answer::Dismiss);
    assert_eq!(
        notice.kind,
        QuestionKind::RemoteMassDelete {
            deletions: 1,
            affected_files: 15,
            space_files: 20,
            exceeded: vec![Limit::Share],
        }
    );
    assert_eq!(notice.items, []);
    let step = b.folder.requested();
    b.take(step).unwrap();
    assert_eq!(b.folder.view().questions, 0);
    let mut store = Store::open(&site_b.db).unwrap();
    assert_eq!(store.transaction(|tx| tx.questions()).unwrap(), vec![]);
}

#[test]
fn a_delete_given_while_stopped_covers_only_the_deletions_its_question_listed() {
    let mut remote = Remote::new();
    let a = answered_while_stopped(&mut remote, Answer::Delete);

    assert!(
        remote.names().contains(&"0.txt".to_owned()),
        "{:?}",
        remote.names()
    );
    assert!(
        a.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(
        a.folder.view().questions,
        1,
        "asked again about d and 0.txt"
    );
}

#[test]
fn a_restore_given_while_stopped_holds_a_deletion_made_since() {
    let mut remote = Remote::new();
    let a = answered_while_stopped(&mut remote, Answer::Restore);

    assert!(
        remote.names().contains(&"0.txt".to_owned()),
        "{:?}",
        remote.names()
    );
    assert!(
        a.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(a.folder.view().questions, 1, "asked about 0.txt");
}

#[test]
fn a_taken_answer_survives_a_second_restart_before_it_is_applied() {
    let mut remote = Remote::new();
    let (site, a) = with_a_held_folder(&mut remote);
    drop(a);
    answer(&site, Answer::Delete);
    let a = site.start();
    assert_eq!(
        a.folder.engine.commits.questions()[0].answer,
        Some(Answer::Delete)
    );
    drop(a);
    fs::remove_file(site.root.join("0.txt")).unwrap();

    let mut a = restarted(&site, &mut remote);
    a.settle();
    a.run(&mut remote);

    assert!(
        remote.names().contains(&"0.txt".to_owned()),
        "{:?}",
        remote.names()
    );
    assert!(
        a.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(a.folder.view().questions, 1);
}

/// The guard holds 11 of the 20 files in `d`, then the person deletes the rest of `d`. The
/// guard asks again under a new id about `d` alone, and the new question continues the first one's lineage, so the
/// daemon does not tell it again.
#[test]
fn a_held_folder_that_takes_the_place_of_its_held_children_continues_their_question() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("d")).unwrap();
    for i in 0..20 {
        fs::write(site.root.join(format!("d/{i}.txt")), format!("in d {i}")).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names().len(), 21);
    for i in 0..11 {
        fs::remove_file(site.root.join(format!("d/{i}.txt"))).unwrap();
    }
    a.scan_until(|a| a.folder.view().questions == 1);
    let [first] = &shown(&site)[..] else {
        panic!("one question");
    };
    let first = first.clone();
    assert_eq!(first.items.len(), 11);
    assert_eq!(first.lineage, first.id);

    fs::remove_dir_all(site.root.join("d")).unwrap();
    a.scan_until(|_| shown(&site).iter().all(|question| question.id != first.id));

    let [second] = &shown(&site)[..] else {
        panic!("one question");
    };
    let held: Vec<_> = (second.items.iter())
        .map(|item| (item.path.unquoted(), item.files))
        .collect();
    assert_eq!(held, [("d", 20)]);
    assert_ne!(second.id, first.id);
    assert_eq!(second.lineage, first.id);
}

/// The person answers restore, the files come back, and the same 11 files are deleted again.
/// The new question holds the same files but starts its own lineage, so the daemon tells it even when it never saw
/// the first one close.
#[test]
fn a_question_asked_again_about_the_same_files_after_an_answer_starts_its_own_lineage() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    for i in 0..20 {
        fs::write(site.root.join(format!("{i}.txt")), format!("file {i}")).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names().len(), 20);
    for i in 0..11 {
        fs::remove_file(site.root.join(format!("{i}.txt"))).unwrap();
    }
    a.scan_until(|a| a.folder.view().questions == 1);
    let first = answer(&site, Answer::Restore);
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(files(&site.root).len(), 20);
    assert_eq!(a.folder.view().questions, 0);

    for i in 0..11 {
        fs::remove_file(site.root.join(format!("{i}.txt"))).unwrap();
    }
    a.scan_until(|a| a.folder.view().questions == 1);

    let [second] = &shown(&site)[..] else {
        panic!("one question");
    };
    let paths = |question: &crate::question::Question| -> Vec<String> {
        question
            .items
            .iter()
            .map(|item| item.path.clone().into())
            .collect()
    };
    assert_eq!(paths(second), paths(&first));
    assert_ne!(second.id, first.id);
    assert_eq!(second.lineage, second.id);
}

/// GUN-423: the server loses `two.txt`'s commit, and c commits `three.txt` to the restored server, so its
/// head is back at b's cursor when b reconnects. b's Resume names the commit it saw at that seq, which the server no
/// longer has, so b lists the tree again instead of resuming on a fork: it gets `three.txt`, and `two.txt`,
/// which b still holds, is created again.
#[test]
fn a_cursor_the_restored_head_caught_up_with_lists_the_tree_again() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    a.create(&mut remote, &site_a.root.join("two.txt"), b"two");
    let (site_b, mut b) = linked(&mut remote, "b");
    b.settle();
    assert_eq!(names(&site_b.root), ["one.txt", "two.txt"]);
    let (site_c, mut c) = linked(&mut restored, "c");
    c.create(&mut restored, &site_c.root.join("three.txt"), b"three");
    let welcome = restored.space.welcome(&b.caller);
    assert_eq!(
        welcome.head_seq,
        b.folder.engine.cursor.as_ref().unwrap().last_seq
    );
    b.event(Event::Disconnected {
        reason: koidrive_net::DisconnectReason::Timeout,
        retry_in: Duration::from_secs(1),
    })
    .unwrap();
    b.event(Event::Connected(welcome)).unwrap();
    for _ in 0..3 {
        b.pull(&restored);
        b.run(&mut restored);
        b.settle();
    }
    c.pull(&restored);
    c.run(&mut restored);
    c.create(&mut restored, &site_c.root.join("four.txt"), b"four");
    for _ in 0..2 {
        b.pull(&restored);
        b.run(&mut restored);
        b.settle();
    }
    assert_eq!(names(&site_b.root), restored.names());
    assert_eq!(
        names(&site_b.root),
        ["four.txt", "one.txt", "three.txt", "two.txt"]
    );
}

#[test]
fn a_held_folder_of_one_file_shows_as_a_folder() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    for (dir, count) in [("d", 15), ("e", 1)] {
        fs::create_dir(site.root.join(dir)).unwrap();
        for i in 0..count {
            fs::write(
                site.root.join(format!("{dir}/{i}.txt")),
                format!("in {dir} {i}"),
            )
            .unwrap();
        }
    }
    for i in 0..4 {
        fs::write(site.root.join(format!("{i}.txt")), format!("beside {i}")).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    drop(a);
    for dir in ["d", "e"] {
        fs::remove_dir_all(site.root.join(dir)).unwrap();
    }
    restarted(&site, &mut remote).rest(&mut remote);

    let [question] = shown(&site).try_into().unwrap();
    let items: Vec<_> = (question.items.iter())
        .map(|item| {
            let kind = serde_json::to_value(item).unwrap()["kind"].clone();
            (item.path.unquoted(), kind, item.files)
        })
        .collect();
    let folder = serde_json::json!("folder");
    assert_eq!(items, [("d", folder.clone(), 15), ("e", folder, 1)]);

    let mut earlier = question.clone();
    for item in &mut earlier.items {
        item.kind = None;
    }
    let body = serde_json::to_vec(&earlier).unwrap();
    (Store::open(&site.db).unwrap())
        .transaction(|tx| tx.reshow_question(&question.id, &body))
        .unwrap();
    restarted(&site, &mut remote).rest(&mut remote);
    assert_eq!(shown(&site), [question], "a row an earlier build stored");
}
