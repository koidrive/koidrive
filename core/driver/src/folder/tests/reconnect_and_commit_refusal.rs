// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_restart_during_a_bootstrap_reads_the_tree_again() {
    for listed in [false, true] {
        let mut remote = Remote::new();
        let (site_a, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");

        let site = Site::new(&remote, "b");
        let mut b = site.start();
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        assert!(matches!(b.work.pop_front(), Some(Work::List)));
        if listed {
            let step = b.folder.listed(Ok(Listed::Done {
                pages: remote.pages(TREE_PAGE),
                trash: vec![],
            }));
            b.take(step).unwrap();
            assert!(matches!(b.work.front(), Some(Work::Subscribe { .. })));
        }
        drop(b);

        let mut b = site.start();
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        assert_eq!(
            b.run(&mut remote),
            ["list", "subscribe", "stage"],
            "listed: {listed}"
        );
        b.settle();
        assert!(b.work.is_empty());
        assert_eq!(files(&site.root), notes());
        assert_eq!(b.state(), FolderState::UpToDate);
    }
}

#[test]
fn a_reconnect_while_a_commit_is_sent_starts_no_second_send() {
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

        a.event(Event::Disconnected {
            reason: koidrive_net::DisconnectReason::Timeout,
            retry_in: Duration::from_secs(1),
        })
        .unwrap();
        a.connect(&remote);
        assert!(
            !a.work.iter().any(|work| matches!(work, Work::Send(_))),
            "reached the server: {reached_the_server}"
        );

        let result = remote.commit(&a.caller, &commit).unwrap();
        let step = a.folder.sent(Ok(result));
        a.take(step).unwrap();
        a.pull(&remote);
        a.run(&mut remote);
        a.settle();
        assert!(a.work.is_empty());
        assert_eq!(remote.space.head_seq(), 1);
        assert_eq!(remote.names(), ["notes.txt"]);
        assert_eq!(a.state(), FolderState::UpToDate);
    }
}

#[test]
fn a_commit_too_large_to_send_is_split() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    for name in ["1.txt", "2.txt", "3.txt", "4.txt"] {
        fs::write(site.root.join(name), name).unwrap();
    }
    a.scan_until(|a| a.folder.engine.trees.local().iter().count() == 4 && !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    assert_eq!(commit.ops.len(), 4);
    let step = a.folder.sent(Err(CallError::TooLarge { bytes: 9 << 20 }));
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    let sizes: Vec<usize> = a.sent.iter().map(|commit| commit.ops.len()).collect();
    assert_eq!(sizes, [2, 2]);
    assert_eq!(remote.names(), ["1.txt", "2.txt", "3.txt", "4.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_read_only_device_keeps_downloading() {
    let mut remote = Remote::new();
    let (site_b, mut b) = linked(&mut remote, "b");
    let (site, mut a) = linked(&mut remote, "a");
    a.caller.role = Role::Viewer;
    a.connect(&remote);
    a.pull(&remote);
    assert_eq!(a.folder.engine.commits.halted(), Some(CommitHalt::ReadOnly));
    fs::write(site.root.join("mine.txt"), b"never sent").unwrap();
    a.scan_until(|a| {
        (a.folder.engine.trees.local().iter()).any(|(_, node)| node.shape.name == "mine.txt")
    });
    assert!(a.work.is_empty());

    b.create(
        &mut remote,
        &site_b.root.join("theirs.txt"),
        b"written on b",
    );
    a.pull(&remote);
    a.run(&mut remote);
    a.settle();
    assert_eq!(
        files(&site.root),
        [
            ("mine.txt".to_owned(), "never sent".to_owned()),
            ("theirs.txt".to_owned(), "written on b".to_owned())
        ]
    );
    assert_eq!(remote.names(), ["theirs.txt"]);
}

#[test]
fn a_refused_commit_lifted_to_resend_goes_out_unchanged() {
    let mut remote = Remote::new();
    let (site, refused, halt) = refused_first_commit(&mut remote, Code::Forbidden);
    assert!(halt.to_string().contains("koi folder retry"), "{halt}");
    assert_eq!(halt.remedy(), crate::status::Remedy::Retry);
    let a = restarted(&site, &mut remote);
    assert!(a.sent.is_empty(), "a restart lifts nothing");
    let problem = a.folder.view().problem.unwrap();
    assert!(problem.contains("koi folder retry"), "{problem}");
    drop(a);

    lift_halt(&site.db, Lift::Resend).unwrap();
    let mut a = restarted(&site, &mut remote);
    assert_eq!(a.sent[0].commit_id, refused.commit_id);
    assert_eq!(remote.names(), ["notes.txt"]);
    a.settle();
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(a.folder.view().problem, None);
}

#[test]
fn a_refused_commit_lifted_to_replan_goes_out_as_a_new_commit_in_its_slot() {
    let mut remote = Remote::new();
    let (site, refused, _) = refused_first_commit(&mut remote, Code::InvalidArgument);
    lift_halt(&site.db, Lift::Replan).unwrap();
    let mut a = restarted(&site, &mut remote);
    assert!(!a.sent.is_empty());
    assert!(
        a.sent
            .iter()
            .all(|sent| sent.commit_id != refused.commit_id)
    );
    assert_eq!(a.sent[0].device_seq, refused.device_seq);
    assert_eq!(remote.names(), ["notes.txt"]);
    a.settle();
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_commit_that_waits_for_the_owners_confirmation_is_not_lifted() {
    let mut remote = Remote::new();
    let (site, refused, halt) = refused_first_commit(&mut remote, Code::ConfirmationRequired);
    assert!(halt.to_string().contains("owner must confirm"), "{halt}");
    assert_eq!(halt.remedy(), crate::status::Remedy::Owner);
    for lift in [Lift::Resend, Lift::Replan] {
        let lifted = lift_halt(&site.db, lift);
        assert!(
            matches!(lifted, Err(DriverError::NeedsConfirmation)),
            "{lifted:?}"
        );
    }
    let mut a = restarted(&site, &mut remote);
    assert!(a.sent.is_empty());

    let (site_b, mut b) = linked(&mut remote, "b");
    b.create(
        &mut remote,
        &site_b.root.join("theirs.txt"),
        b"written on b",
    );
    a.pull(&remote);
    a.run(&mut remote);
    a.settle();
    assert!(
        files(&site.root).contains(&("theirs.txt".to_owned(), "written on b".to_owned())),
        "{:?}",
        files(&site.root)
    );
    assert!(a.sent.is_empty());
    let flight = a.folder.engine.commits.in_flight().unwrap();
    assert_eq!(flight.commit.commit_id, refused.commit_id);
}

/// GUN-349. Device `b` is moving `old.txt` to `remote.txt` after `a` renamed it, when `a` renames it back and the
/// user edits it on `b`. Only the edit was the user's, so the file ends as `old.txt` with the edit on both sides.
#[test]
fn a_file_edited_while_a_move_the_server_undid_was_in_flight_keeps_its_name() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("old.txt"), b"one");
    let (site_b, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::rename(site_a.root.join("old.txt"), site_a.root.join("remote.txt")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    assert!(matches!(
        b.folder.engine.effects.in_flight().first().unwrap().effect,
        koidrive_engine::effects::Effect::Move { .. }
    ));
    fs::rename(site_a.root.join("remote.txt"), site_a.root.join("old.txt")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    fs::write(site_b.root.join("old.txt"), b"user edit").unwrap();
    b.scan_until(|b| {
        (b.folder.engine.trees.local().iter())
            .any(|(_, node)| (node.shape.content.as_ref()).is_some_and(|content| content.size == 9))
    });
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        files(&site_b.root),
        [("old.txt".to_owned(), "user edit".to_owned())]
    );
    assert_eq!(remote.names(), ["old.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_space_this_device_can_no_longer_reach_is_not_lifted_by_a_restart() {
    for reason in [StopReason::Forbidden, StopReason::NotFound] {
        assert_eq!(
            Stop::Session(reason).remedy(),
            crate::status::Remedy::Membership
        );
    }
}

#[test]
fn a_commit_resent_after_a_restart_uploads_again_only_once_its_receipts_no_longer_hold() {
    for (expired, unstored, uploads) in [(false, false, 0), (true, false, 1), (false, true, 1)] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        for n in 0..60 {
            fs::write(site.root.join(format!("{n}.txt")), format!("file {n}")).unwrap();
        }
        a.rescan();
        a.scan_until(|a| !a.work.is_empty());
        let upload = a.work.pop_front().unwrap();
        a.perform(upload, &mut remote).unwrap();
        let Some(Work::Send(commit)) = a.work.pop_front() else {
            panic!("the commit is sent after its upload");
        };
        assert_eq!(commit.receipts.len(), 60);
        drop(a);

        if unstored {
            let mut store = Store::open(&site.db).unwrap();
            store
                .transaction(|tx| {
                    let mut engine = Engine::load(tx, &site.link.space_id)?;
                    engine.commits.uploaded(Vec::new());
                    engine.checkpoint(tx)
                })
                .unwrap();
        }
        if expired {
            remote.now_ms += 3_600_001;
        }
        let mut a = site.start();
        a.connect(&remote);
        a.scan_until(|a| a.folder.scanned);
        let ran = a.run(&mut remote);
        a.rest(&mut remote);
        let case = format!("expired: {expired}, unstored: {unstored}");
        let again = ran.iter().filter(|work| **work == "upload").count();
        assert_eq!(again, uploads, "{case}");
        let sends = (a.sent.iter()).filter(|sent| sent.commit_id == commit.commit_id);
        assert_eq!(sends.count(), uploads + 1, "{case}");
        assert_eq!(remote.space.head_seq(), 1, "{case}");
        assert_eq!(remote.names().len(), 60, "{case}");
        assert_eq!(a.state(), FolderState::UpToDate, "{case}");
    }
}
