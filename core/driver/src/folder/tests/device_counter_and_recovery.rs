// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_new_store_on_a_device_that_synced_the_space_before_goes_on_from_the_servers_count() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    drop(a);

    let again = Site::new(&remote, "a");
    let mut a = again.start();
    a.connect(&remote);
    assert_eq!(a.folder.engine.commits.accepted(), 1);
    a.scan_until(|a| a.folder.scanned);
    assert_eq!(a.run(&mut remote), ["list", "subscribe", "stage"]);
    a.create(
        &mut remote,
        &again.root.join("more.txt"),
        b"from the folder linked again",
    );
    assert_eq!(a.sent[0].device_seq, 2);
    assert_eq!(remote.names(), ["more.txt", "notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_history_changed_resync_names_the_seq_even_when_it_is_0() {
    let mut remote = Remote::new();
    let (_site, mut a) = linked(&mut remote, "a");
    a.event(Event::ResyncRequired(ResyncRequired {
        reason: Reason::HistoryChanged as i32,
        restore_seq: 0,
        restore_point_ms: 1_794_442_424_000,
    }))
    .unwrap();
    let recovery = a.folder.view().recovery.expect("the folder recovers");
    assert_eq!(recovery.restore_seq, Some(0));
    assert_eq!(
        describe(&recovery),
        "the space was rewound on the server to seq 0 (requested 2026-11-12 00:13 UTC)"
    );
}

#[test]
fn a_never_synced_store_takes_the_servers_count_only_at_its_first_welcome() {
    let mut remote = Remote::new();
    let site = Site::new(&remote, "a");
    let mut a = site.start();
    a.connect(&remote);
    assert_eq!(a.folder.engine.commits.accepted(), 0);

    let welcome = Welcome {
        device_seq: 1,
        ..remote.space.welcome(&a.caller)
    };
    a.event(Event::Connected(welcome)).unwrap();
    assert!(matches!(
        a.folder.engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));
    assert_eq!(a.folder.engine.commits.accepted(), 0);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    assert_eq!(a.folder.engine.commits.halted(), None);
    assert_eq!(a.folder.engine.commits.accepted(), 1);
}

#[test]
fn a_store_that_took_the_servers_count_does_not_take_another() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    drop(a);

    let again = Site::new(&remote, "a");
    let mut a = again.start();
    a.connect(&remote);
    assert_eq!(a.folder.engine.commits.accepted(), 1);
    assert_eq!(a.folder.engine.cursor, None);
    drop(a);

    let mut a = again.start();
    let welcome = Welcome {
        device_seq: 2,
        ..remote.space.welcome(&a.caller)
    };
    a.event(Event::Connected(welcome)).unwrap();
    assert!(matches!(
        a.folder.engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));
    assert_eq!(a.folder.engine.commits.accepted(), 1);
    assert!(matches!(a.work.front(), Some(Work::List { .. })));
}

#[test]
fn a_commit_from_the_store_a_new_store_replaced_is_refused_by_the_server() {
    let mut remote = Remote::new();
    let (old, commit) = stopped_with_a_commit_in_flight(&mut remote);
    let (new, mut a) = linked(&mut remote, "a");
    a.create(
        &mut remote,
        &new.root.join("more.txt"),
        b"from the new store",
    );
    assert_eq!(a.sent[0].device_seq, commit.device_seq);
    drop(a);

    // The server counts one commit more than the old store acknowledged, which is what a lost response looks like,
    // so the old store sends its commit again. The slot is the new store's by now.
    let mut stale = old.start();
    stale.connect(&remote);
    let Some(Work::Send(again)) = stale.work.pop_front() else {
        panic!("the stored commit is sent again");
    };
    assert_eq!(again.commit_id, commit.commit_id);
    let refusal = remote.commit(&stale.caller, &again).unwrap_err();
    assert!(
        matches!(&refusal, CallError::Server(error) if error.code() == Code::StaleDeviceState),
        "{refusal}"
    );
    let step = stale.folder.sent(Err(refusal));
    stale.take(step).unwrap();
    assert!(matches!(
        stale.folder.engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));
    stale.run(&mut remote);
    stale.settle();
    stale.run(&mut remote);
    assert_eq!(stale.folder.engine.commits.halted(), None);
    assert_eq!(remote.space.head_seq(), 2);
    assert_eq!(remote.names(), ["more.txt", "notes.txt"]);
    assert_eq!(names(&old.root), ["draft.txt", "more.txt", "notes.txt"]);
    assert_eq!(stale.folder.view().questions, 1);
}

#[test]
fn a_rewound_space_recovers_at_welcome_and_names_the_seq_it_was_rewound_to() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    let welcome = Welcome {
        history_id: "rewound".into(),
        restore_seq: 7,
        restore_point_ms: 1_794_442_424_000,
        ..remote.space.welcome(&a.caller)
    };
    a.event(Event::Connected(welcome)).unwrap();
    let recovery = a.folder.view().recovery.expect("the folder recovers");
    assert_eq!(recovery, RecoveryCause::rewind(7, 1_794_442_424_000));
    assert_eq!(
        describe(&recovery),
        "the space was rewound on the server to seq 7 (requested 2026-11-12 00:13 UTC)"
    );
}

#[test]
fn a_recovery_at_welcome_drops_the_wait_of_the_commit_it_abandons() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    let refusal = koidrive_proto::v1::Error {
        code: Code::Internal as i32,
        message: "try again".into(),
        ..Default::default()
    };
    let step = a.folder.sent(Err(CallError::Server(ServerError(refusal))));
    a.take(step).unwrap();
    assert!(a.folder.view().wait.is_some());

    let welcome = Welcome {
        history_id: "rewound".into(),
        ..remote.space.welcome(&a.caller)
    };
    a.event(Event::Connected(welcome)).unwrap();
    assert!(a.folder.view().recovery.is_some());
    assert_eq!(a.folder.view().wait, None);
}

#[test]
fn a_history_changed_resync_on_a_live_session_recovers_through_a_new_connection() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("kept.txt"), b"before");
    let backup = remote.space.clone();
    a.create(&mut remote, &site.root.join("rewound.txt"), b"after");
    remote.space.rewind(&backup, NOW);

    a.pull(&remote);
    assert_eq!(
        a.work
            .iter()
            .last()
            .map(|work| matches!(work, Work::Reconnect)),
        Some(true)
    );
    assert_eq!(a.run(&mut remote).first(), Some(&"reconnect"));
    a.settle();
    a.run(&mut remote);
    assert_ne!(a.state(), FolderState::Connecting);
    assert_eq!(a.folder.view().questions, 1);
    assert_eq!(
        a.folder.view().recovery.map(|recovery| recovery.trigger),
        Some(RecoveryTrigger::SpaceRewound)
    );
    assert_eq!(remote.names(), ["kept.txt"]);
    assert_eq!(names(&site.root), ["kept.txt", "rewound.txt"]);
}

#[test]
fn a_recovery_question_records_the_rewind_it_comes_from() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("kept.txt"), b"before");
    let backup = remote.space.clone();
    a.create(&mut remote, &site.root.join("rewound.txt"), b"after");
    remote.space.rewind(&backup, NOW);
    a.pull(&remote);
    assert_eq!(a.run(&mut remote).first(), Some(&"reconnect"));
    a.settle();
    a.run(&mut remote);
    assert_eq!(a.folder.view().questions, 1);
    drop(a);
    let question = answer(&site, Answer::Publish);
    assert_eq!(
        question.recovery_cause.map(|cause| cause.trigger),
        Some(crate::RecoveryTrigger::SpaceRewound)
    );
}

#[test]
fn an_upload_that_ends_after_a_recovery_leaves_the_next_commit_in_flight() {
    use koidrive_proto::v1::error::Code;
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("kept.txt"), b"kept");
    let backup = remote.space.clone();
    remote.space.rewind(&backup, NOW);
    fs::write(site.root.join("draft.txt"), b"uploading at the rewind").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(Work::Upload(stale)) = a.work.pop_front() else {
        panic!("draft.txt is uploaded");
    };
    let receipts = remote.store(&a.caller, &stale);

    a.connect(&remote);
    assert!(matches!(
        a.folder.engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));
    fs::remove_file(site.root.join("draft.txt")).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(a.folder.engine.commits.halted(), None);
    fs::create_dir(site.root.join("folder")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the new folder is sent");
    };

    let step = a.folder.uploaded(Ok(receipts));
    a.take(step).unwrap();
    let step = a
        .folder
        .sent(Err(CallError::Server(put_off(Code::RetryLater))));
    a.take(step).unwrap();
    assert_eq!(a.run(&mut remote), ["wait", "send"]);
    assert_eq!(remote.names(), ["folder", "kept.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_synced_store_recovers_when_the_server_counts_a_commit_it_does_not_hold() {
    let mut remote = Remote::new();
    // The old store read the space and committed nothing: it holds a cursor, and its count is 0 like a new store's.
    let (old, a) = linked(&mut remote, "a");
    assert!(a.folder.engine.cursor.is_some());
    drop(a);
    let (new, mut a) = linked(&mut remote, "a");
    a.create(
        &mut remote,
        &new.root.join("more.txt"),
        b"from the new store",
    );
    drop(a);

    let mut stale = old.start();
    let welcome = remote.space.welcome(&stale.caller);
    assert_eq!(welcome.device_seq, 1);
    stale.event(Event::Connected(welcome)).unwrap();
    assert_eq!(stale.folder.engine.commits.accepted(), 0);
    assert!(matches!(
        stale.work.iter().collect::<Vec<_>>()[..],
        [Work::List { .. }]
    ));
    stale.scan_until(|a| a.folder.scanned);
    stale.run(&mut remote);
    stale.settle();
    stale.run(&mut remote);
    assert_eq!(names(&old.root), ["more.txt"]);
    assert_eq!(stale.folder.engine.commits.accepted(), 1);
    stale.create(&mut remote, &old.root.join("last.txt"), b"after recovery");
    assert_eq!(stale.sent.last().unwrap().device_seq, 2);
    assert_eq!(remote.names(), ["last.txt", "more.txt"]);
    assert_eq!(stale.state(), FolderState::UpToDate);
}

#[test]
fn a_store_with_a_commit_in_flight_recovers_and_holds_it_when_the_server_counts_past_its_slot() {
    let mut remote = Remote::new();
    let (old, commit) = stopped_with_a_commit_in_flight(&mut remote);
    let (new, mut a) = linked(&mut remote, "a");
    a.create(
        &mut remote,
        &new.root.join("more.txt"),
        b"from the new store",
    );
    a.create(&mut remote, &new.root.join("again.txt"), b"and once more");
    drop(a);

    let mut stale = old.start();
    let welcome = remote.space.welcome(&stale.caller);
    assert_eq!(welcome.device_seq, 3);
    stale.event(Event::Connected(welcome)).unwrap();
    assert!(
        !stale.work.iter().any(|work| matches!(work, Work::Send(_))),
        "the commit is not sent again"
    );
    assert_eq!(stale.folder.engine.commits.accepted(), 1);
    stale.scan_until(|a| a.folder.scanned);
    stale.run(&mut remote);
    stale.settle();
    stale.run(&mut remote);
    let commits = &stale.folder.engine.commits;
    assert_eq!(commits.accepted(), 3);
    assert!(commits.in_flight().is_none());
    assert_eq!(
        names(&old.root),
        ["again.txt", "draft.txt", "more.txt", "notes.txt"]
    );
    assert_eq!(remote.names(), ["again.txt", "more.txt", "notes.txt"]);

    let question = answer(&old, Answer::Publish);
    assert_eq!(
        question.kind,
        crate::question::QuestionKind::Recovery {
            group: crate::question::RecoveryGroup::CreatedOnThisDevice,
            sent_in: None,
            cause: Some(RecoveryCause::of(RecoveryTrigger::StoreRolledBack)),
        }
    );
    let step = stale.folder.requested();
    stale.take(step).unwrap();
    stale.run(&mut remote);
    stale.settle();
    stale.run(&mut remote);
    assert_ne!(stale.sent.last().unwrap().commit_id, commit.commit_id);
    assert_eq!(stale.sent.last().unwrap().device_seq, 4);
    assert_eq!(
        remote.names(),
        ["again.txt", "draft.txt", "more.txt", "notes.txt"]
    );
    assert_eq!(stale.state(), FolderState::UpToDate);
}

#[test]
fn a_recovery_after_a_refusal_in_the_session_continues_from_the_servers_count() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    a.create(&mut remote, &site.root.join("two.txt"), b"two");
    fs::write(site.root.join("three.txt"), b"three").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    assert!(matches!(a.work.pop_front(), Some(Work::Send(_))));
    let step = a.folder.sent(Err(CallError::Server(ServerError(ProtoError {
        code: Code::StaleDeviceState as i32,
        ..Default::default()
    }))));
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    answer(&site, Answer::Publish);
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(a.sent.last().unwrap().device_seq, 3);
    assert_eq!(remote.names(), ["one.txt", "three.txt", "two.txt"]);
}

#[test]
fn a_rewind_on_the_server_holds_the_writes_it_took_until_the_person_keeps_them() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(
        &mut remote,
        &site.root.join("kept.txt"),
        b"before the restore point",
    );
    let backup = remote.space.clone();
    a.create(
        &mut remote,
        &site.root.join("rewound.txt"),
        b"after the restore point",
    );
    remote.space.rewind(&backup, NOW);

    a.event(Event::ResyncRequired(ResyncRequired {
        reason: Reason::HistoryChanged as i32,
        ..ResyncRequired::default()
    }))
    .unwrap();
    assert!(matches!(
        a.folder.engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["kept.txt", "rewound.txt"]);
    assert_eq!(remote.names(), ["kept.txt"]);

    let question = answer(&site, Answer::Keep);
    assert_eq!(
        question.kind,
        crate::question::QuestionKind::Recovery {
            group: crate::question::RecoveryGroup::CreatedAfterRestorePoint,
            sent_in: None,
            cause: Some(RecoveryCause::rewind(0, 0)),
        }
    );
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), ["kept.txt", "rewound.txt"]);
    assert_eq!(names(&site.root), ["kept.txt", "rewound.txt"]);
    assert_eq!(a.folder.view().questions, 0);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_store_older_than_the_disk_recovers_even_when_the_server_lost_its_commits() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    drop(a);
    let copy = site.db.with_extension("copy");
    fs::copy(&site.db, &copy).unwrap();
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    let mut a = site.start();
    a.connect(&remote);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["one.txt", "two.txt"]);
    drop(a);
    fs::copy(&copy, &site.db).unwrap();
    let restored = Remote::new();
    assert_eq!(restored.space.welcome(&site.caller).device_seq, 0);

    let lock = claim::lock(&site.root)
        .unwrap()
        .expect("the folder is not locked");
    let local = local_rules(&SyncRules::default()).unwrap();
    let folder = Folder::open(site.link.clone(), &lock, &site.db, local).unwrap();
    assert!(matches!(
        folder.engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));
}

fn forgotten_then_renamed(truncated: bool) -> (Site, Remote, Device) {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    drop(a);
    let copy = site.db.with_extension("copy");
    fs::copy(&site.db, &copy).unwrap();
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    let mut a = site.start();
    a.connect(&remote);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["one.txt", "two.txt"]);
    drop(a);
    fs::rename(other.root.join("two.txt"), other.root.join("three.txt")).unwrap();
    b.scan_until(|device| !device.work.is_empty());
    b.run(&mut remote);
    assert_eq!(remote.names(), ["one.txt", "three.txt"]);
    if truncated {
        let head = remote.space.head_seq();
        remote.space.truncate_log(head);
    }
    fs::copy(&copy, &site.db).unwrap();
    let mut a = site.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    (site, remote, a)
}

#[test]
fn a_file_a_store_older_than_the_disk_forgot_follows_the_rename_the_server_made_since() {
    let (site, remote, a) = forgotten_then_renamed(false);
    assert_eq!(names(&site.root), ["one.txt", "three.txt"]);
    assert!(shown(&site).is_empty());
    assert_eq!(remote.names(), ["one.txt", "three.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_file_a_store_older_than_the_disk_forgot_is_asked_about_when_the_log_no_longer_reaches_the_store() {
    let (site, _, _) = forgotten_then_renamed(true);
    assert_eq!(names(&site.root), ["one.txt", "three.txt", "two.txt"]);
    let kinds: Vec<_> = shown(&site).into_iter().map(|question| question.kind).collect();
    assert_eq!(
        kinds,
        [crate::question::QuestionKind::Recovery {
            group: crate::question::RecoveryGroup::CreatedOnThisDevice,
            sent_in: None,
            cause: Some(RecoveryCause::of(RecoveryTrigger::StoreOlderThanDisk)),
        }]
    );
}

#[test]
fn a_welcome_during_a_recovery_keeps_its_cause() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    drop(a);
    let copy = site.db.with_extension("copy");
    fs::copy(&site.db, &copy).unwrap();
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    let mut a = site.start();
    a.connect(&remote);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["one.txt", "two.txt"]);
    drop(a);
    fs::copy(&copy, &site.db).unwrap();

    let mut a = site.start();
    let older_than_disk = Some(RecoveryCause::of(RecoveryTrigger::StoreOlderThanDisk));
    assert_eq!(a.folder.view().recovery, older_than_disk);
    a.connect(&remote);
    assert_eq!(a.folder.view().recovery, older_than_disk);
}

#[test]
fn a_recovery_keeps_its_cause_across_a_restart() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    drop(a);
    let copy = site.db.with_extension("copy");
    fs::copy(&site.db, &copy).unwrap();
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    let mut a = site.start();
    a.connect(&remote);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    drop(a);
    fs::copy(&copy, &site.db).unwrap();
    drop(site.start());

    let mut a = site.start();
    let older_than_disk = Some(RecoveryCause::of(RecoveryTrigger::StoreOlderThanDisk));
    assert_eq!(a.folder.view().recovery, older_than_disk);
    a.connect(&remote);
    assert_eq!(a.folder.view().recovery, older_than_disk);
}
