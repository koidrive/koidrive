// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_new_file_becomes_one_commit_one_upload_and_one_send() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    assert_eq!(a.state(), FolderState::UpToDate);

    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.state(), FolderState::CatchingUp);
    assert_eq!(a.run(&mut remote), ["upload", "send"]);
    a.settle();
    assert!(a.work.is_empty());

    let [commit] = a.sent.as_slice() else {
        panic!("one commit, not {}", a.sent.len());
    };
    assert_eq!(kinds(commit), ["create file"]);
    assert_eq!(commit.receipts.len(), 1);
    assert_eq!(remote.names(), ["notes.txt"]);
    let trees = &a.folder.engine.trees;
    let (id, local) = trees.local().iter().next().expect("L holds the file");
    assert_eq!(trees.synced().get(id), Some(&local.shape));
    assert_eq!(
        trees.remote().get(id).map(|node| &node.shape),
        Some(&local.shape)
    );
    assert!(trees.dirty().is_empty());
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(a.folder.view().last_seq, Some(1));
}

#[test]
fn a_commit_result_is_acknowledged_once_whether_the_call_or_the_stream_brings_it_first() {
    for call_first in [true, false] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
        a.scan_until(|a| !a.work.is_empty());
        let upload = a.work.pop_front().unwrap();
        a.perform(upload, &mut remote).unwrap();
        let Some(Work::Send(commit)) = a.work.pop_front() else {
            panic!("the commit is sent after its upload");
        };
        let result = remote.commit(&a.caller, &commit).unwrap();
        let by_stream = Event::Committed(Committed {
            commit_id: commit.commit_id.clone(),
            result: result.clone(),
        });
        if call_first {
            let step = a.folder.sent(Ok(result));
            a.take(step).unwrap();
            a.event(by_stream).unwrap();
        } else {
            a.event(by_stream).unwrap();
            let step = a.folder.sent(Ok(result));
            a.take(step).unwrap();
        }
        assert!(a.work.is_empty(), "call first: {call_first}");
        assert!(a.folder.engine.commits.in_flight().is_none());
        assert_eq!(a.state(), FolderState::UpToDate);

        a.create(&mut remote, &site.root.join("more.txt"), b"the next commit");
        assert_eq!(a.sent.last().unwrap().device_seq, 2);
        assert_eq!(remote.space.head_seq(), 2, "call first: {call_first}");
        assert_eq!(a.state(), FolderState::UpToDate);
    }
}

/// RFC 0013 §6: a server from before commit tokens sends results with no time. The cursor then
/// holds no token at the result's seq instead of a commit ID paired with time 0, which a server
/// that checks tokens would take for another commit.
#[test]
fn a_result_without_a_time_leaves_the_cursor_token_unknown() {
    for legacy in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "legacy-result");
        fs::write(site.root.join("notes.txt"), b"a retained commit").unwrap();
        a.scan_until(|a| !a.work.is_empty());
        let upload = a.work.pop_front().unwrap();
        a.perform(upload, &mut remote).unwrap();
        let Some(Work::Send(commit)) = a.work.pop_front() else {
            panic!("the commit is sent after its upload");
        };
        let mut result = remote.commit(&a.caller, &commit).unwrap();
        if legacy {
            result.committed_at_ms = 0;
        }
        let step = a.folder.sent(Ok(result));
        a.take(step).unwrap();
        let cursor = site.stored().cursor.unwrap();
        assert_eq!(cursor.last_seq, 1);
        assert_eq!(cursor.commit_id.is_empty(), legacy);
        let since = resume(&cursor.history_id, &cursor.base());
        assert!(
            remote.space.resume(&since).is_ok(),
            "legacy={legacy}, resume={since:?}"
        );
    }
}

#[test]
fn a_remote_file_becomes_one_effect_and_its_observation_changes_nothing() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage"]);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"written on a"
    );
    // The effect's acknowledgement settled the trees in its own transaction, so S holds the file as L does before
    // anything is observed or arrives from the log. Otherwise the next change of the file would count as concurrent.
    let folder = &mut b.folder;
    let (id, local) = (folder.engine.trees.local().iter().next()).expect("L holds the file");
    assert_eq!(folder.engine.trees.synced().get(id), Some(&local.shape));
    let stored = (folder.store)
        .transaction(|tx| Engine::load(tx, &folder.link.space_id))
        .unwrap();
    assert_eq!(stored.trees.synced().get(id), Some(&local.shape));
    b.settle();
    assert!(b.work.is_empty());
    assert!(b.sent.is_empty());
    assert!(b.folder.engine.trees.dirty().is_empty());
    assert_eq!(b.folder.engine.effects.cleared(), 1);
    assert_eq!(b.state(), FolderState::UpToDate);
    assert_eq!(remote.space.head_seq(), 1);
}

#[test]
fn a_download_the_server_puts_off_for_an_hour_shows_in_the_status_until_it_goes_through() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    *remote.chunks.refusal.lock().unwrap() = Some(ServerError(koidrive_proto::v1::Error {
        code: koidrive_proto::v1::error::Code::QuotaExceeded as i32,
        message: "the daily limit is used up".into(),
        retry_after_ms: 3_600_000,
        ..Default::default()
    }));
    let pending = loop {
        match b.work.pop_front().expect("an effect is staged") {
            Work::Stage(pending) => break pending,
            other => b.perform(other, &mut remote).unwrap(),
        }
    };
    let staged = ready(pending.stage(&remote.chunks));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    let problem = b
        .folder
        .view()
        .problem
        .expect("the wait shows in the status");
    assert!(problem.contains("QUOTA_EXCEEDED"), "{problem}");
    let wait = b
        .folder
        .view()
        .wait
        .expect("the wait is typed in the status");
    assert_eq!(wait.kind, WaitKind::Quota);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(wait.until_ms.is_some_and(|at| at > now_ms + 3_500_000), "{wait:?}");

    *remote.chunks.refusal.lock().unwrap() = None;
    assert_eq!(b.run(&mut remote), ["stage again"]);
    assert_eq!(b.folder.view().problem, None);
    assert_eq!(b.folder.view().wait, None);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"written on a"
    );
}

#[test]
fn a_download_that_fails_for_now_stages_the_same_effect_again() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    *remote.chunks.unavailable.lock().unwrap() = 1;
    let pending = loop {
        match b.work.pop_front().expect("an effect is staged") {
            Work::Stage(pending) => break pending,
            other => b.perform(other, &mut remote).unwrap(),
        }
    };
    let planned = b
        .folder
        .engine
        .effects
        .in_flight()
        .first()
        .unwrap()
        .effect_id
        .clone();
    let staged = ready(pending.stage(&remote.chunks));
    assert!(matches!(staged, Err(Stall::Later { .. })));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    b.settle();
    assert!(matches!(b.work.make_contiguous(), [Work::StageAfter(..)]));
    assert_eq!(
        b.folder
            .engine
            .effects
            .in_flight()
            .first()
            .unwrap()
            .effect_id,
        planned
    );

    assert_eq!(b.run(&mut remote), ["stage again"]);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"written on a"
    );
    assert_eq!(b.folder.engine.effects.cleared(), 1);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_local_change_is_committed_only_after_the_effect_in_flight() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    let pending = loop {
        match b.work.pop_front().expect("an effect is staged") {
            Work::Stage(pending) => break pending,
            other => b.perform(other, &mut remote).unwrap(),
        }
    };
    fs::write(site.root.join("mine.txt"), b"written on b").unwrap();
    b.scan_until(|b| b.folder.engine.trees.local().len() == 1);
    assert!(
        b.work.is_empty(),
        "no commit is built while the effect is staged"
    );
    assert!(b.folder.engine.commits.in_flight().is_none());

    b.work.push_back(Work::Stage(pending));
    assert_eq!(b.run(&mut remote), ["stage", "upload", "send"]);
    b.settle();
    assert_eq!(names(&site.root), ["mine.txt", "notes.txt"]);
    assert_eq!(remote.names(), ["mine.txt", "notes.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_remote_change_is_applied_only_after_the_commit_in_flight() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    fs::write(site.root.join("mine.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(send) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };

    b.create(&mut remote, &site_b.root.join("notes.txt"), b"written on b");
    a.pull(&remote);
    assert_eq!(a.folder.engine.trees.remote().len(), 1, "R holds b's file");
    assert!(
        a.work.is_empty(),
        "no effect is planned while the commit is in flight"
    );
    assert!(a.folder.engine.effects.in_flight().is_empty());

    a.work.push_back(send);
    let ran = a.run(&mut remote);
    assert_eq!(ran.last(), Some(&"stage"), "{ran:?}");
    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(names(&site.root), ["mine.txt", "notes.txt"]);
    assert_eq!(remote.names(), ["mine.txt", "notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_tree_listing_put_off_for_an_hour_shows_in_the_status_and_one_put_off_for_a_second_does_not() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert!(matches!(b.work.pop_front(), Some(Work::List)));

    let later = |wait, why: &str, kind| Listed::Later {
        listing: Listing::default(),
        wait,
        why: format!("reading the space's tree failed: {why}"),
        kind,
    };
    let step = b.folder.listed(Ok(later(
        Duration::from_secs(1),
        "RETRY_LATER: busy",
        WaitKind::Retry,
    )));
    b.take(step).unwrap();
    assert_eq!(b.folder.view().problem, None);
    assert_eq!(
        b.folder.view().wait.map(|wait| wait.kind),
        Some(WaitKind::Retry)
    );
    assert!(matches!(b.work.pop_front(), Some(Work::ListAfter(..))));

    let quota = "QUOTA_EXCEEDED: the daily limit is used up";
    let step = b.folder.listed(Ok(later(
        Duration::from_secs(3_600),
        quota,
        WaitKind::Quota,
    )));
    b.take(step).unwrap();
    assert_eq!(
        b.folder.view().wait.map(|wait| wait.kind),
        Some(WaitKind::Quota)
    );
    let problem = b
        .folder
        .view()
        .problem
        .expect("an hour's wait shows in the status");
    assert!(problem.contains(quota), "{problem}");
    assert!(problem.ends_with(" UTC"), "{problem}");

    b.run(&mut remote);
    assert_eq!(b.folder.view().problem, None);
    assert_eq!(b.folder.view().wait, None);
    assert_eq!(names(&site.root), ["one.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_quota_wait_of_a_second_is_typed_in_the_status_without_a_problem() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert!(matches!(b.work.pop_front(), Some(Work::List)));

    let step = b.folder.listed(Ok(Listed::Later {
        listing: Listing::default(),
        wait: Duration::from_secs(1),
        why: "reading the space's tree failed: QUOTA_EXCEEDED: the daily limit is used up".into(),
        kind: WaitKind::Quota,
    }));
    b.take(step).unwrap();

    let view = b.folder.view();
    assert_eq!(view.problem, None);
    let wait = view
        .wait
        .expect("a wait of a second is typed in the status");
    assert_eq!(wait.kind, WaitKind::Quota);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(wait.until_ms.is_some_and(|at| at <= now_ms + 1_000), "{wait:?}");

    b.run(&mut remote);
    assert_eq!(b.folder.view().wait, None);
    assert_eq!(names(&site.root), ["one.txt"]);
}

#[test]
fn an_upload_put_off_for_an_hour_leaves_the_status_once_its_file_is_deleted() {
    use koidrive_proto::v1::error::Code;
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let path = site.root.join("notes.txt");
    fs::write(&path, b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(Work::Upload(upload)) = a.work.pop_front() else {
        panic!("the file is uploaded");
    };
    let hash = upload.chunks[0].hash.clone();
    let refused = ChunkError::Call(CallError::Server(put_off(Code::QuotaExceeded)));
    let step = a.folder.uploaded(Err(refused));
    a.take(step).unwrap();
    assert!(
        a.folder.view().problem.is_some(),
        "the wait shows in the status"
    );

    fs::remove_file(&path).unwrap();
    a.scan_until(|a| a.folder.engine.trees.local().is_empty());
    let wait = a.work.pop_front().unwrap();
    a.perform(wait, &mut remote).unwrap();
    assert!(matches!(a.work.pop_front(), Some(Work::Upload(_))));
    let step = a.folder.uploaded(Err(ChunkError::SourceChanged { hash }));
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(a.folder.view().problem, None);
}

#[test]
fn a_commit_put_off_for_an_hour_leaves_the_status_once_the_server_refuses_it() {
    use koidrive_proto::v1::error::Code;
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    let step = a
        .folder
        .sent(Err(CallError::Server(put_off(Code::RetryLater))));
    a.take(step).unwrap();
    assert!(
        a.folder.view().problem.is_some(),
        "the wait shows in the status"
    );

    let wait = a.work.pop_front().unwrap();
    a.perform(wait, &mut remote).unwrap();
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the commit is sent again");
    };
    let step = a
        .folder
        .sent(Err(CallError::Server(put_off(Code::HeadMoved))));
    a.take(step).unwrap();
    assert_eq!(a.folder.view().problem, None);
    assert_eq!(a.run(&mut remote), ["upload", "send"]);
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_bootstrap_whose_pages_were_read_at_two_heads_replaces_r_after_the_replay() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    let first = remote.space.list_tree(&ListTree {
        cursor: String::new(),
        limit: 1,
    });
    a.create(&mut remote, &site_a.root.join("two.txt"), b"two");
    let mut pages = vec![first];
    while !pages.last().unwrap().next_cursor.is_empty() {
        let cursor = pages.last().unwrap().next_cursor.clone();
        pages.push(remote.space.list_tree(&ListTree { cursor, limit: 1 }));
    }
    let (first_head, last_head) = (pages[0].head_seq, pages.last().unwrap().head_seq);
    assert!(first_head < last_head);

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert!(matches!(b.work.pop_front(), Some(Work::List)));
    let step = b.folder.listed(Ok(Listed::Done {
        pages,
        trash: vec![],
    }));
    b.take(step).unwrap();
    assert!(matches!(
        b.work.front(),
        Some(Work::Subscribe(Resume { since_seq, .. })) if *since_seq == first_head
    ));
    b.event(Event::CaughtUp(CaughtUp {
        head_seq: first_head,
        ..Default::default()
    }))
    .unwrap();
    assert_eq!(b.state(), FolderState::Bootstrapping);
    assert_eq!(b.folder.engine.cursor, None);
    assert!(b.folder.engine.trees.remote().is_empty());

    assert_eq!(b.run(&mut remote), ["subscribe", "stage"]);
    assert_eq!(b.folder.view().last_seq, Some(last_head));
    assert_eq!(names(&site.root), ["one.txt", "two.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_truncated_log_reads_the_tree_again_and_keeps_what_is_synced() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    assert_eq!(names(&site.root), ["one.txt"]);

    a.create(&mut remote, &site_a.root.join("two.txt"), b"two");
    a.create(&mut remote, &site_a.root.join("three.txt"), b"three");
    remote.space.truncate_log(remote.space.head_seq());
    b.pull(&remote);
    assert_eq!(b.state(), FolderState::Bootstrapping);
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage"]);
    assert_eq!(names(&site.root), ["one.txt", "three.txt", "two.txt"]);
    assert_eq!(b.folder.engine.effects.cleared(), 3);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn an_edit_after_a_restart_is_an_edit_of_the_synced_file() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    a.settle();
    drop(a);

    let mut a = site.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    assert_eq!(a.run(&mut remote), ["subscribe"]);
    a.settle();
    assert!(a.work.is_empty(), "a restart finds nothing to sync");
    assert_eq!(a.state(), FolderState::UpToDate);

    a.create(
        &mut remote,
        &site.root.join("notes.txt"),
        b"edited after the restart",
    );
    assert_eq!(kinds(&a.sent[0]), ["update content"]);
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn an_in_place_edit_made_while_stopped_that_keeps_the_size_and_mtime_is_sent_after_a_restart() {
    for outside_link in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
        a.settle();
        let path = site.root.join("notes.txt");
        let outside = site.root.with_file_name("outside.txt");
        if outside_link {
            fs::hard_link(&path, &outside).unwrap();
            a.rescan();
            a.settle();
        }
        drop(a);

        if outside_link {
            fs::remove_file(&outside).unwrap();
        }
        edit_keeping_the_size_and_mtime(&path);

        let mut a = site.start();
        a.connect(&remote);
        a.settle();
        assert_eq!(
            a.run(&mut remote),
            ["subscribe", "upload", "send"],
            "{outside_link}"
        );
        assert_eq!(kinds(&a.sent[0]), ["update content"]);
        assert_eq!(a.state(), FolderState::UpToDate);
    }
}

#[test]
fn an_edit_made_while_a_new_file_with_an_outside_link_is_read_is_sent_after_a_restart() {
    use std::os::unix::fs::PermissionsExt;

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    let path = site.root.join("copy.txt");
    let outside = site.root.with_file_name("outside.txt");
    fs::write(&path, b"written on a").unwrap();
    fs::hard_link(&path, &outside).unwrap();
    a.rescan();
    take_one_observation(&mut a, || {
        fs::remove_file(&outside).unwrap();
        edit_keeping_the_size_and_mtime(&path);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    });
    drop(a);

    let mut a = site.start();
    a.connect(&remote);
    a.rest(&mut remote);
    assert_eq!(kinds(a.sent.last().unwrap()), ["update content"]);
}

fn take_one_observation(a: &mut Device, meanwhile: impl FnOnce()) {
    let stamp = a.folder.sections().stamp();
    let observed = Observer::observe(&mut a.observer, Duration::from_millis(500)).unwrap();
    meanwhile();
    if let Some(observation) = observed {
        let unread = a.observer.unread();
        let batch = crate::disk::Batch {
            stamp,
            answered: Default::default(),
            observation,
            unread,
        };
        let entries = a.observer.snapshot().entries.len() as u64;
        let step = a.folder.observed(batch, entries);
        a.take(step).unwrap();
    }
}

fn edit_keeping_the_size_and_mtime(path: &Path) {
    use std::io::Write;

    let written = fs::metadata(path).unwrap().modified().unwrap();
    let mut file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.write_all(b"edited on a!").unwrap();
    file.set_modified(written).unwrap();
}

#[test]
fn an_upgraded_store_reports_only_files_changed_after_its_last_write_before_the_upgrade() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("kept.txt"), b"kept on a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    a.settle();
    drop(a);
    (rusqlite::Connection::open(&site.db).unwrap())
        .execute_batch(
            "ALTER TABLE local_nodes DROP COLUMN ctime_ns;
             DELETE FROM migrations WHERE name = 'local_nodes_ctime';",
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));

    let path = site.root.join("notes.txt");
    edit_keeping_the_size_and_mtime(&path);

    let mut a = site.start();
    let mut snapshot = a.folder.engine.snapshot();
    let found = snapshot.rescan(&site.root, [PathBuf::new()]).unwrap();
    assert_eq!(
        found.changes,
        [koidrive_fs::Change::Modified {
            path: "notes.txt".into()
        }]
    );
    a.connect(&remote);
    a.settle();
    assert_eq!(a.run(&mut remote), ["subscribe", "upload", "send"]);
    assert_eq!(kinds(&a.sent[0]), ["update content"]);
}

#[test]
fn an_uploaded_file_is_read_again_only_once_for_an_attribute_another_app_wrote() {
    #[cfg(target_vendor = "apple")]
    const ATTRIBUTE: &str = "com.example.tag";
    #[cfg(not(target_vendor = "apple"))]
    const ATTRIBUTE: &str = "user.example.tag";
    let reads = |a: &Device| {
        (a.reports.lock().unwrap().iter())
            .filter(|report| matches!(report, Observing::Reading { bytes, .. } if *bytes > 0))
            .count()
    };
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let path = site.root.join("notes.txt");
    a.create(&mut remote, &path, b"written on a");
    let uploaded = reads(&a);
    a.settle();
    assert_eq!(reads(&a), uploaded, "the node mark reads nothing again");

    rustix::fs::setxattr(&path, ATTRIBUTE, b"red", rustix::fs::XattrFlags::empty()).unwrap();
    a.settle();
    assert_eq!(reads(&a), uploaded + 1);
    assert!(a.run(&mut remote).is_empty());
    drop(a);
    let mut snapshot = site.start().folder.engine.snapshot();
    assert_eq!(
        snapshot
            .rescan(&site.root, [PathBuf::new()])
            .unwrap()
            .changes,
        []
    );
}

/// Design §2.7 leaves a node the server refused an op for out of commits until the next rescan. The first whole scan
/// of a start is that rescan (GUN-345). Nothing cleared the node before, so it never synced again.
#[test]
fn a_node_the_server_refused_is_committed_again_after_the_scan_of_the_next_start() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    assert_eq!(kinds(&commit), ["create file"]);
    let refusal = koidrive_proto::v1::Error {
        code: Code::LimitExceeded as i32,
        ..Default::default()
    };
    let step = a.folder.sent(Err(CallError::Server(ServerError(refusal))));
    a.take(step).unwrap();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);

    fs::write(site.root.join("notes.txt"), b"edited on a").unwrap();
    a.settle();
    assert!(a.work.is_empty(), "the refused node stays out of commits");
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    assert!(remote.names().is_empty());
    drop(a);

    let mut a = site.start();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    assert!(a.folder.engine.commits.problems().is_empty());
    assert_eq!(a.run(&mut remote), ["subscribe", "upload", "send"]);
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn an_effect_that_failed_at_a_name_taken_for_a_moment_is_planned_again_after_a_wait() {
    let mut remote = Remote::new();
    let (site, mut b, first) = failed_at_a_taken_name(&mut remote);
    assert!(first <= Duration::from_secs(1), "{first:?}");

    // The user's file is gone before the observer saw it, so L is as it was and the engine plans the same effect.
    fs::remove_file(site.root.join("notes.txt")).unwrap();
    b.settle();
    assert!(b.work.is_empty(), "the node waits");
    assert_eq!(b.folder.engine.effects.blocked().len(), 1);

    let step = b.folder.retry();
    b.take(step).unwrap();
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the same effect is planned again");
    };
    let second = take_the_name_and_apply(&remote, &site, &mut b, pending);
    assert!(second >= Duration::from_secs(1), "{second:?}");

    fs::remove_file(site.root.join("notes.txt")).unwrap();
    let step = b.folder.retry();
    b.take(step).unwrap();
    assert_eq!(b.run(&mut remote), ["stage"]);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"written on a"
    );
    assert!(b.folder.engine.effects.blocked().is_empty());
    b.settle();
    assert!(b.work.is_empty());
    assert_eq!(b.state(), FolderState::UpToDate);
}

/// The block was stored, so it outlived the process. A start plans the effect of every blocked node again.
#[test]
fn a_start_plans_an_effect_that_failed_before_it_again() {
    let mut remote = Remote::new();
    let (site, b, _) = failed_at_a_taken_name(&mut remote);
    drop(b);
    fs::remove_file(site.root.join("notes.txt")).unwrap();
    assert_eq!(site.stored().effects.blocked().len(), 1);

    let mut b = site.start();
    assert!(b.folder.engine.effects.blocked().is_empty());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"written on a"
    );
    b.settle();
    assert!(b.work.is_empty());
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_restart_with_a_commit_in_flight_sends_the_stored_commit_again() {
    for reached_the_server in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
        a.scan_until(|a| !a.work.is_empty());
        let upload = a.work.pop_front().unwrap();
        a.perform(upload, &mut remote).unwrap();
        let Some(Work::Send(commit)) = a.work.pop_front() else {
            panic!("the commit is sent after its upload");
        };
        if reached_the_server {
            remote.commit(&a.caller, &commit).unwrap();
        }
        drop(a);

        let mut a = site.start();
        a.connect(&remote);
        assert!(
            matches!(a.work.front(), Some(Work::Send(again)) if *again == commit),
            "reached the server: {reached_the_server}"
        );
        a.scan_until(|a| a.folder.scanned);
        a.run(&mut remote);
        a.settle();
        assert!(a.work.is_empty());
        assert!(a.sent.iter().all(|sent| sent.commit_id == commit.commit_id));
        assert_eq!(remote.space.head_seq(), 1);
        assert_eq!(remote.names(), ["notes.txt"]);
        assert_eq!(a.state(), FolderState::UpToDate);
    }
}

#[test]
fn a_file_that_changes_while_it_is_uploaded_is_committed_with_its_new_bytes() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let path = site.root.join("notes.txt");
    fs::write(&path, b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(Work::Upload(upload)) = a.work.pop_front() else {
        panic!("the content is uploaded first");
    };
    fs::write(
        &path,
        b"and written again, longer, before the upload read it",
    )
    .unwrap();
    let chunk = upload.chunks[0].clone();
    assert!(!koidrive_net::matches(
        &chunk,
        &upload.read(&chunk).unwrap()
    ));
    let step = a
        .folder
        .uploaded(Err(ChunkError::SourceChanged { hash: chunk.hash }));
    a.take(step).unwrap();
    assert!(a.folder.engine.commits.in_flight().is_none());
    assert!(matches!(a.work.make_contiguous(), [Work::ResendAfter(_)]));

    let size = |tree: &Tree<LocalNode>| {
        (tree.iter()).find_map(|(_, node)| node.shape.content.as_ref().map(|content| content.size))
    };
    let on_disk = Some(fs::metadata(&path).unwrap().len());
    a.scan_until(|a| size(a.folder.engine.trees.local()) == on_disk);
    assert_eq!(a.run(&mut remote), ["wait", "upload", "send"]);
    a.settle();
    assert!(a.work.is_empty());
    let in_r = (a.folder.engine.trees.remote().iter())
        .find_map(|(_, node)| node.shape.content.as_ref().map(|content| content.size));
    assert_eq!(in_r, on_disk);
    assert_eq!(remote.space.head_seq(), 1);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_chunk_sent_again_after_a_refused_ticket_counts_its_bytes_once() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    for i in 0..3 {
        fs::write(site.root.join(format!("{i}.txt")), format!("file {i}")).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    let Some(Work::Upload(upload)) = a.work.pop_front() else {
        panic!("the content is uploaded first");
    };
    let distinct: BTreeMap<_, _> = (upload.chunks.iter())
        .map(|chunk| (chunk.hash.clone(), u64::from(chunk.size)))
        .collect();
    assert_eq!(distinct.len(), 3);

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (result, seen) = runtime.block_on(async {
        let (relay, seen) = crate::scripted::relay_refusing(1, 1).await;
        let (session, _events, api) = crate::scripted::session(relay).await;
        let transfer = Transfer::new(&api);
        let mut bytes = 0;
        let receipts = upload_counted(&session, &transfer, upload, |size| bytes += size).await;
        ((receipts, bytes), seen)
    });
    let (receipts, bytes) = result;
    assert_eq!(receipts.unwrap().len(), 3);
    assert_eq!(
        seen.lock().unwrap().hashes.len(),
        4,
        "the refused chunk is sent again"
    );
    assert_eq!(bytes, distinct.values().sum::<u64>());
}

#[test]
fn small_files_go_in_one_bundle_to_a_server_that_offers_bundles_and_one_put_each_to_one_that_does_not() {
    for offered in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        for i in 0..5 {
            fs::write(site.root.join(format!("{i}.txt")), format!("file {i}")).unwrap();
        }
        a.scan_until(|a| !a.work.is_empty());
        let Some(Work::Upload(upload)) = a.work.pop_front() else {
            panic!("the content is uploaded first");
        };
        let total: u64 = upload.chunks.iter().map(|chunk| u64::from(chunk.size)).sum();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let ((receipts, bytes), seen) = runtime.block_on(async {
            let (relay, seen) = crate::scripted::relay(1).await;
            let features = if offered { vec![koidrive_net::CHUNK_BUNDLES.into()] } else { Vec::new() };
            let (session, _events, api) = crate::scripted::session_with(relay, features).await;
            let transfer = Transfer::new(&api);
            let mut bytes = 0;
            let receipts = upload_counted(&session, &transfer, upload, |size| bytes += size).await;
            ((receipts, bytes), seen)
        });
        assert_eq!(receipts.unwrap().len(), 5);
        assert_eq!(bytes, total);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.stored.len(), 5);
        let expected: &[usize] = if offered { &[5] } else { &[] };
        assert_eq!(seen.bundles, expected, "bundles offered: {offered}");
    }
}

#[test]
fn an_upload_that_fails_after_storing_some_chunks_counts_the_bytes_of_those() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    for i in 0..3 {
        fs::write(site.root.join(format!("{i}.txt")), format!("file {i}")).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    let Some(Work::Upload(upload)) = a.work.pop_front() else {
        panic!("the content is uploaded first");
    };
    let sizes: BTreeMap<_, _> = (upload.chunks.iter())
        .map(|chunk| (chunk.hash.clone(), u64::from(chunk.size)))
        .collect();
    assert_eq!(sizes.len(), 3);
    let refused = upload.chunks.last().unwrap().hash.clone();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (result, seen) = runtime.block_on(async {
        let (relay, seen) = crate::scripted::relay(1).await;
        seen.lock().unwrap().refuse = Some(refused.clone());
        let (session, _events, api) = crate::scripted::session(relay).await;
        let mut transfer = Transfer::new(&api);
        transfer.attempts = 1;
        transfer.backoff = koidrive_net::Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(1),
        };
        let mut bytes = 0;
        let receipts = upload_counted(&session, &transfer, upload, |size| bytes += size).await;
        ((receipts, bytes), seen)
    });
    let (receipts, bytes) = result;
    assert!(receipts.is_err(), "the refused chunk ends the upload");
    let stored = seen.lock().unwrap().stored.clone();
    assert_eq!(stored.len(), 2);
    assert!(!stored.contains(&refused));
    assert_eq!(bytes, stored.iter().map(|hash| sizes[hash]).sum::<u64>());
}

#[test]
fn with_four_uploads_at_once_a_file_that_changed_after_it_was_chunked_abandons_the_commit() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    for i in 0..8 {
        let mut bytes = format!("file {i}").into_bytes();
        bytes.resize(koidrive_net::FULL_CHUNK_BYTES as usize - 64, 0);
        fs::write(site.root.join(format!("{i}.txt")), bytes).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    let Some(Work::Upload(upload)) = a.work.pop_front() else {
        panic!("the content is uploaded first");
    };
    assert_eq!(upload.chunks.len(), 8);
    let changed = vec![b'x'; koidrive_net::FULL_CHUNK_BYTES as usize - 32];
    let last = upload.chunks.last().unwrap();
    let name = (0..8)
        .map(|i| format!("{i}.txt"))
        .find(|name| koidrive_net::matches(last, &fs::read(site.root.join(name)).unwrap()))
        .unwrap();
    fs::write(site.root.join(name), &changed).unwrap();
    let stale = (upload.chunks.iter())
        .find(|chunk| !koidrive_net::matches(chunk, &upload.read(chunk).unwrap()))
        .unwrap()
        .hash
        .clone();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (result, seen) = runtime.block_on(async {
        let (relay, seen) = crate::scripted::relay(4).await;
        let (session, _events, api) = crate::scripted::session(relay).await;
        let mut transfer = Transfer::new(&api);
        transfer.gate = UploadGate::new(4);
        (upload_chunks(&session, &transfer, upload).await, seen)
    });
    assert!(
        matches!(&result, Err(ChunkError::SourceChanged { hash }) if *hash == stale),
        "{result:?}"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.peak, 4,
        "the relay saw these chunks before the changed one ended the upload: {:?}",
        seen.hashes
    );
    assert!(!seen.hashes.contains(&stale));
    drop(seen);

    let step = a.folder.uploaded(result);
    a.take(step).unwrap();
    assert!(a.folder.engine.commits.in_flight().is_none());
    assert!(matches!(a.work.make_contiguous(), [Work::ResendAfter(_)]));
    let holds_the_new_bytes = |tree: &Tree<LocalNode>| {
        (tree.iter()).any(|(_, node)| {
            (node.shape.content.as_ref())
                .is_some_and(|content| content.size == changed.len() as u64)
        })
    };
    a.scan_until(|a| holds_the_new_bytes(a.folder.engine.trees.local()));
    assert_eq!(a.run(&mut remote), ["wait", "upload", "send"]);
    a.settle();
    assert!((a.folder.engine.trees.remote().iter()).any(|(_, node)| {
        (node.shape.content.as_ref()).is_some_and(|content| content.size == changed.len() as u64)
    }));
    assert_eq!(remote.names().len(), 8);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_restart_reads_nothing_after_any_change_the_observer_accepted() {
    use std::os::unix::fs::PermissionsExt;

    let reads = || crate::observe::READS.take();
    type Change = fn(&Site, &mut Device, &mut Remote);
    let rows: [(&str, &[&str], &[&str], Change); 7] = [
        ("an edit", &["notes.txt"], &[], |site, a, remote| {
            fs::write(site.root.join("notes.txt"), b"edited on a!").unwrap();
            a.rest(remote);
        }),
        (
            "a mark the next poll sees",
            &["marked.txt"],
            &[],
            |site, a, remote| {
                fs::write(site.root.join("marked.txt"), b"marked on a").unwrap();
                a.scan_until(|a| !a.work.is_empty());
                assert_eq!(a.run(remote), ["upload", "send"]);
                take_one_observation(a, || {});
            },
        ),
        ("a rename", &[], &[], |site, _, _| {
            fs::rename(site.root.join("notes.txt"), site.root.join("renamed.txt")).unwrap();
        }),
        ("a chmod", &[], &[], |site, _, _| {
            let path = site.root.join("notes.txt");
            let mode = fs::metadata(&path).unwrap().permissions().mode() ^ 0o020;
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        }),
        ("a link added", &[], &[], |site, _, _| {
            fs::hard_link(
                site.root.join("notes.txt"),
                site.root.with_file_name("outside.txt"),
            )
            .unwrap();
        }),
        (
            "edited once unlinked",
            &["notes.txt", "notes.txt"],
            &[],
            |site, a, _| {
                let outside = site.root.with_file_name("outside.txt");
                fs::hard_link(site.root.join("notes.txt"), &outside).unwrap();
                a.rescan();
                a.settle();
                fs::remove_file(&outside).unwrap();
                edit_keeping_the_size_and_mtime(&site.root.join("notes.txt"));
            },
        ),
        (
            "a link removed",
            &["link.txt", "notes.txt"],
            &[],
            |site, a, remote| {
                fs::hard_link(site.root.join("notes.txt"), site.root.join("link.txt")).unwrap();
                a.rest(remote);
                fs::remove_file(site.root.join("link.txt")).unwrap();
            },
        ),
    ];
    let mut wrong = Vec::new();
    for (row, read, read_again, change) in rows {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
        a.settle();
        reads();
        change(&site, &mut a, &mut remote);
        a.rest(&mut remote);
        let live = reads();
        drop(a);

        let mut a = site.start();
        a.connect(&remote);
        let mut polls = 0;
        while !a.folder.scanned || polls < 5 || !a.observer.racy_reads().is_empty() {
            assert!(polls < 600, "{row}: the restarted observer never settles");
            polls += 1;
            let sections = a.folder.sections();
            if let Some(batch) = sections
                .observe(&mut a.observer, Duration::from_millis(100))
                .unwrap()
            {
                let entries = a.observer.snapshot().entries.len() as u64;
                let step = a.folder.observed(batch, entries);
                a.take(step).unwrap();
            }
            if !a.folder.scanned && a.observer.scanned().is_some() {
                let step = a.folder.scanned();
                a.take(step).unwrap();
            }
        }
        let seen = reads();
        let paths = |read: &[&str]| read.iter().map(PathBuf::from).collect::<Vec<_>>();
        if (&live, &seen) != (&paths(read), &paths(read_again)) {
            wrong.push(format!(
                "{row}: read {live:?} then {seen:?} after a restart, {read:?} then {read_again:?} expected"
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

fn saved_unchanged(restart: bool) -> (Remote, Site, Device, Site, Device) {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"synced");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    let notes = site.root.join("notes.txt");
    let before = disk_mtime(&notes);
    fs::write(&notes, b"synced").unwrap();
    assert_ne!(disk_mtime(&notes), before);
    if restart {
        drop(b);
        b = site.start();
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
    }
    for _ in 0..2 {
        b.settle();
        b.run(&mut remote);
    }
    (remote, site_a, a, site, b)
}

fn disk_mtime(path: &Path) -> i64 {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).unwrap();
    meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
}

fn idle_plans(b: &mut Device, remote: &mut Remote) -> usize {
    let plans = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = plans.clone();
    b.folder.disk.set_hook(Box::new(move |cut| {
        if cut == crate::disk::Cut::AfterPlan {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }));
    for _ in 0..10 {
        b.scan(Duration::from_millis(100));
        b.run(remote);
    }
    plans.load(std::sync::atomic::Ordering::SeqCst)
}

#[test]
fn a_file_saved_unchanged_takes_its_new_modification_time_and_sends_nothing() {
    for restart in [false, true] {
        let (_, _, _, site, b) = saved_unchanged(restart);

        let trees = &b.folder.engine.trees;
        let id = local_named(&b, "notes.txt");
        let mtime = |shape: &koidrive_engine::tree::Shape| shape.content.as_ref().unwrap().mtime_ns;
        let node = trees.local().get(&id).unwrap();
        let local = mtime(&node.shape);
        assert_eq!(local, disk_mtime(&site.root.join("notes.txt")), "{restart}");
        assert!(node.content_from_server, "{restart}");
        assert!(!trees.unsent_edits().contains(&id), "{restart}");
        let remote = trees.remote().get(&id).map(|node| &node.shape);
        assert_eq!(trees.synced().get(&id), remote, "{restart}");
        assert_ne!(mtime(remote.unwrap()), local, "{restart}");
        assert!(b.sent.is_empty(), "{restart}: {:?}", b.sent);
        assert_eq!(b.state(), FolderState::UpToDate, "{restart}");
    }
}

#[test]
fn a_file_saved_unchanged_goes_to_the_trash_when_another_device_deletes_it() {
    for restart in [false, true] {
        let (mut remote, site_a, mut a, site, mut b) = saved_unchanged(restart);

        fs::remove_file(site_a.root.join("notes.txt")).unwrap();
        a.scan_until(|a| !a.work.is_empty());
        a.run(&mut remote);
        for _ in 0..3 {
            b.pull(&remote);
            b.run(&mut remote);
            b.settle();
        }

        assert_eq!(idle_plans(&mut b, &mut remote), 0, "{restart}");
        assert!(!site.root.join("notes.txt").exists(), "{restart}");
        assert_eq!(trashed(&site.root).len(), 1, "{restart}");
        assert!(b.sent.is_empty(), "{restart}: {:?}", b.sent);
        assert_eq!(b.state(), FolderState::UpToDate, "{restart}");
    }
}

#[test]
fn an_edit_saved_again_unchanged_while_it_waits_to_upload_uploads_once() {
    for again in [false, true] {
        let (mut remote, _, _, site, mut b) = saved_unchanged(false);
        let notes = site.root.join("notes.txt");

        fs::write(&notes, b"edited").unwrap();
        b.scan_until(|b| !b.work.is_empty());
        if again {
            fs::write(&notes, b"edited").unwrap();
            b.settle();
        }
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);

        let id = local_named(&b, "notes.txt");
        assert_eq!(server_bytes(&remote, &id), b"edited", "{again}");
        let edits = (b.sent.iter().flat_map(|commit| &commit.ops)).count();
        assert_eq!(edits, 1, "{again}: {:?}", b.sent);
        assert_eq!(idle_plans(&mut b, &mut remote), 0, "{again}");
        assert_eq!(b.state(), FolderState::UpToDate, "{again}");
    }
}
