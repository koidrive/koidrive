// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_first_link_records_its_proven_cause_with_the_stored_question() {
    let mut remote = Remote::new();
    let (site, b) = a_first_link_with_a_name_clash(&mut remote, "notes.txt", "Notes.txt");
    drop(b);
    let question = answer(&site, Answer::Publish);
    assert_eq!(
        question.recovery_cause,
        Some(crate::RecoveryCause {
            trigger: crate::RecoveryTrigger::NoSyncState,
            restore_point_ms: None,
        })
    );
    assert_eq!(
        question.kind,
        crate::QuestionKind::Recovery {
            group: crate::RecoveryGroup::CreatedOnThisDevice,
            sent_in: None,
            cause: None,
        }
    );
}

#[test]
fn a_first_link_holds_what_the_space_lacks_and_publishing_it_keeps_both_files_of_one_name() {
    // The space's name and the folder's: in another case, in another Unicode form, and where case folding changes
    // the name's length.
    for (theirs, mine) in [
        ("notes.txt", "Notes.txt"),
        ("r\u{e9}sum\u{e9}.txt", "re\u{301}sume\u{301}.txt"),
        ("stra\u{df}e.txt", "STRASSE.txt"),
    ] {
        let mut remote = Remote::new();
        let (site, mut b) = a_first_link_with_a_name_clash(&mut remote, theirs, mine);

        let question = answer(&site, Answer::Publish);
        assert_eq!(
            question.kind,
            crate::question::QuestionKind::Recovery {
                group: crate::question::RecoveryGroup::CreatedOnThisDevice,
                sent_in: None,
                cause: None,
            }
        );
        let held: Vec<_> = (question.items.iter())
            .map(|item| (item.path.as_str(), item.files))
            .collect();
        let mut expected = [(mine, 1), ("mine.txt", 1)];
        expected.sort();
        assert_eq!(held, expected, "{mine}");
        let step = b.folder.requested();
        b.take(step).unwrap();
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);

        let on_disk = files(&site.root);
        let texts: Vec<&str> = on_disk.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(texts.len(), 3, "{mine}: {on_disk:?}");
        for text in ["already here", "mine", "written on a"] {
            assert!(texts.contains(&text), "{mine}: {on_disk:?}");
        }
        assert!(
            on_disk
                .iter()
                .any(|(path, text)| path == theirs && text == "written on a")
        );
        assert_eq!(remote.names().len(), 3, "{mine}");
        assert_eq!(b.folder.view().questions, 0);
        assert_eq!(b.state(), FolderState::UpToDate);
    }
}

#[test]
fn a_fresh_first_link_sends_what_the_space_lacks_and_keeps_both_files_of_one_name_without_a_question() {
    for (theirs, mine) in [
        ("notes.txt", "Notes.txt"),
        ("r\u{e9}sum\u{e9}.txt", "re\u{301}sume\u{301}.txt"),
        ("stra\u{df}e.txt", "STRASSE.txt"),
        ("notes.txt", "notes.txt"),
    ] {
        let mut remote = Remote::new();
        let (site_a, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site_a.root.join(theirs), b"written on a");
        let site = Site::new(&remote, "b");
        fs::write(site.root.join(mine), b"already here").unwrap();
        fs::write(site.root.join("mine.txt"), b"mine").unwrap();
        let mut b = site.start();
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        sync(&mut remote, (&site_a, &mut a), (&site, &mut b));

        assert_eq!(b.folder.view().questions, 0, "{mine}");
        assert_eq!(a.folder.view().questions, 0, "{mine}");
        assert_eq!(remote.names().len(), 3, "{mine}: {:?}", remote.names());
        for root in [&site.root, &site_a.root] {
            let on_disk = files(root);
            let mut texts: Vec<&str> = on_disk.iter().map(|(_, text)| text.as_str()).collect();
            texts.sort();
            assert_eq!(texts, ["already here", "mine", "written on a"], "{mine}: {on_disk:?}");
            assert!(
                (on_disk.iter()).any(|(path, text)| path == theirs && text == "written on a"),
                "{mine}: {on_disk:?}"
            );
        }
        assert_eq!(b.state(), FolderState::UpToDate);
        assert_eq!(a.state(), FolderState::UpToDate);
    }
}

#[test]
fn discarding_what_a_first_link_holds_moves_it_to_the_local_trash_and_downloads_the_space() {
    let mut remote = Remote::new();
    let (site, mut b) = a_first_link_with_a_name_clash(&mut remote, "notes.txt", "Notes.txt");

    answer(&site, Answer::Discard);
    let step = b.folder.requested();
    b.take(step).unwrap();
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert!(b.sent.is_empty(), "nothing is sent for a discarded entry");
    assert_eq!(
        files(&site.root),
        [("notes.txt".to_owned(), "written on a".to_owned())]
    );
    let mut trash = trashed(&site.root);
    trash.sort();
    assert_eq!(trash.len(), 2, "{trash:?}");
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(b.folder.view().questions, 0);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_question_the_engine_holds_without_its_row_is_shown_and_answered_once_the_folder_opens() {
    let mut remote = Remote::new();
    let (site, b) = a_first_link_with_a_name_clash(&mut remote, "notes.txt", "Notes.txt");
    drop(b);
    let mut store = Store::open(&site.db).unwrap();
    store
        .transaction(|tx| {
            let [stored] = &tx.questions()?[..] else {
                panic!("one question");
            };
            assert!(tx.remove_question(&stored.question_id)?);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
    drop(store);

    let mut b = restarted(&site, &mut remote);
    b.settle();
    b.run(&mut remote);
    assert!(b.sent.is_empty(), "nothing is sent before an answer");
    let question = answer(&site, Answer::Publish);
    assert_eq!(question.items.len(), 2);
    let step = b.folder.requested();
    b.take(step).unwrap();
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(remote.names().len(), 3);
    assert_eq!(b.folder.view().questions, 0);
}

/// Discard on a first link whose held folder also holds an entry L does not know: a file the device's rules exclude,
/// or a symlink. Discard moves the listed file to the local trash and keeps the folder with that entry. The folder's
/// removal is not planned again on a timer, and the status names the entry.
#[test]
fn discarding_a_held_folder_that_holds_an_entry_l_does_not_know_keeps_the_folder_and_names_the_entry()
 {
    for unknown in ["build.log", "link"] {
        let mut remote = Remote::new();
        let site = synced_before(&mut remote, "b");
        let (site_a, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");
        fs::create_dir(site.root.join("held")).unwrap();
        fs::write(site.root.join("held/a.txt"), b"listed").unwrap();
        let entry = site.root.join("held").join(unknown);
        match unknown {
            "link" => std::os::unix::fs::symlink("/etc/hosts", &entry).unwrap(),
            _ => fs::write(&entry, b"left out").unwrap(),
        }
        let sync = SyncRules {
            include: Vec::new(),
            exclude: vec!["*.log".into()],
        };
        let mut b = site.start_with(&sync);
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);
        answer(&site, Answer::Discard);
        let step = b.folder.requested();
        b.take(step).unwrap();
        let mut steps = 0;
        while let Some(work) = b.work.pop_front() {
            steps += 1;
            assert!(
                steps < 20,
                "{unknown}: the removal is planned again and again"
            );
            b.perform(work, &mut remote).unwrap();
        }

        assert!(
            b.sent.is_empty(),
            "{unknown}: nothing is sent for a discarded entry"
        );
        assert!(fs::symlink_metadata(&entry).is_ok(), "{unknown} is kept");
        assert!(!site.root.join("held/a.txt").exists());
        assert_eq!(trashed(&site.root).len(), 1, "{unknown}");
        let view = b.folder.view();
        assert_eq!(
            (view.state, view.questions),
            (FolderState::UpToDate, 0),
            "{unknown}"
        );
        let problem = view.problem.unwrap_or_default();
        assert!(
            problem.contains(&format!("\"held\" (the server's change could not be made here: the folder holds entries this device does not sync: {unknown:?})")),
            "{problem}"
        );
    }
}

#[test]
fn a_first_link_holds_what_another_devices_rules_leave_out_for_a_while_until_the_person_publishes_it()
 {
    let mut remote = Remote::new();
    let site = synced_before(&mut remote, "b");
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");

    fs::write(site.root.join("mine.txt"), b"mine").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(b.folder.view().questions, 1);

    let rules = site_a.root.join(".koiignore");
    for (written, left_out) in [(&b"mine.txt\n"[..], true), (b"other.txt\n", false)] {
        a.create(&mut remote, &rules, written);
        b.pull(&remote);
        b.run(&mut remote);
        b.scan_until(|b| !b.folder.disk.awaits_rules());
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);
        let id = (b.folder.engine.trees.local().iter())
            .find(|(_, node)| node.shape.name == "mine.txt")
            .map(|(id, _)| id.clone())
            .unwrap();
        assert_eq!(b.folder.engine.trees.excluded(&id), left_out);
        assert!(b.sent.is_empty(), "mine.txt waits for the person's answer");
    }
    assert_eq!(remote.names(), [".koiignore", "notes.txt"]);
    assert_eq!(b.folder.view().questions, 1);

    let question = answer(&site, Answer::Publish);
    let held: Vec<_> = (question.items.iter())
        .map(|item| item.path.as_str())
        .collect();
    assert_eq!(held, ["mine.txt"]);
    let step = b.folder.requested();
    b.take(step).unwrap();
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "mine.txt", "notes.txt"]);
    assert_eq!(b.folder.view().questions, 0);
}

#[test]
fn a_first_link_adopts_what_the_space_holds_alike_and_publishes_the_rest_when_asked() {
    let mut remote = Remote::new();
    let site_a = Site::new(&remote, "a");
    fs::write(site_a.root.join("notes.txt"), b"written on a").unwrap();
    fs::create_dir(site_a.root.join("shared")).unwrap();
    fs::write(site_a.root.join("shared/plan.txt"), b"a plan from a").unwrap();
    let mut a = site_a.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    assert_eq!(a.run(&mut remote), ["list", "subscribe", "upload", "send"]);
    assert_eq!(a.folder.view().questions, 0, "the space was empty");

    let site = synced_before(&mut remote, "b");
    let head = remote.space.head_seq();
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    fs::write(site.root.join("mine.txt"), b"already here").unwrap();
    fs::create_dir(site.root.join("shared")).unwrap();
    fs::write(site.root.join("shared/draft.txt"), b"a draft from b").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert!(b.sent.is_empty(), "nothing is sent before an answer");
    assert_eq!(remote.space.head_seq(), head);
    let local = b.folder.engine.trees.local();
    let notes = (local.iter()).find(|(_, node)| node.shape.name == "notes.txt");
    assert!(
        notes.is_some_and(|(id, _)| b.folder.engine.trees.remote().get(id).is_some()),
        "the same notes.txt is adopted as the space's node"
    );
    assert_eq!(
        fs::read(site.root.join("shared/plan.txt")).unwrap(),
        b"a plan from a"
    );

    let question = answer(&site, Answer::Publish);
    let held: Vec<_> = (question.items.iter())
        .map(|item| (item.path.as_str(), item.files))
        .collect();
    assert_eq!(held, [("mine.txt", 1), ("shared/draft.txt", 1)]);
    let step = b.folder.requested();
    b.take(step).unwrap();
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.pull(&remote);
    a.run(&mut remote);
    a.settle();
    assert!(a.work.is_empty());

    let both = [
        ("mine.txt", "already here"),
        ("notes.txt", "written on a"),
        ("shared/draft.txt", "a draft from b"),
        ("shared/plan.txt", "a plan from a"),
    ]
    .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site.root), both);
    assert_eq!(files(&site_a.root), both);
    assert_eq!(remote.space.head_seq(), head + 1);
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(b.state(), FolderState::UpToDate);
}


#[test]
fn a_fresh_first_link_adopts_what_the_space_holds_alike_and_sends_the_rest() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("shared")).unwrap();
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");
    a.create(&mut remote, &site_a.root.join("shared/plan.txt"), b"a plan from a");

    let site = Site::new(&remote, "b");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    fs::write(site.root.join("mine.txt"), b"already here").unwrap();
    fs::create_dir(site.root.join("shared")).unwrap();
    fs::write(site.root.join("shared/draft.txt"), b"a draft from b").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));

    let both = [
        ("mine.txt", "already here"),
        ("notes.txt", "written on a"),
        ("shared/draft.txt", "a draft from b"),
        ("shared/plan.txt", "a plan from a"),
    ]
    .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site.root), both);
    assert_eq!(files(&site_a.root), both);
    assert_eq!(b.folder.view().questions, 0);
    assert_eq!(
        remote.names(),
        ["draft.txt", "mine.txt", "notes.txt", "plan.txt", "shared"]
    );
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_first_link_of_a_folder_whose_marker_counts_effects_holds_what_the_space_lacks() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");
    let site = Site::new(&remote, "b");
    let marker = Marker {
        space_id: remote.space.space_id().to_owned(),
        effects: 3,
    };
    write_marker(&site.root, &marker).unwrap();
    fs::write(site.root.join("old.txt"), b"an old copy").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.rest(&mut remote);
    assert!(b.sent.is_empty(), "nothing is sent before an answer");
    assert_eq!(b.folder.view().questions, 1);
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(names(&site.root), ["notes.txt", "old.txt"]);
}

#[test]
fn a_first_link_of_a_folder_another_device_synced_and_unlinked_keeps_what_the_space_deleted_since_deleted() {
    for (kept, found_marker) in [(true, false), (true, true), (false, false), (false, true)] {
        let mut remote = Remote::new();
        let (site_a, mut a) = linked(&mut remote, "a");
        if kept {
            a.create(&mut remote, &site_a.root.join("kept.txt"), b"kept");
        }
        a.create(&mut remote, &site_a.root.join("old.txt"), b"written on a");
        let old = node_named(&a, "old.txt");
        drop(a);
        let (site_b, mut b) = linked(&mut remote, "b");
        fs::remove_file(site_b.root.join("old.txt")).unwrap();
        b.rescan();
        b.rest(&mut remote);
        let space: Vec<&str> = if kept { vec!["kept.txt"] } else { vec![] };
        assert_eq!(remote.names(), space);
        assert!(remote.space.purge(&old, NOW));
        assert_eq!(koidrive_fs::read_marker(&site_a.root).unwrap().unwrap().effects, 0);

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.sqlite");
        let link = Link {
            device_id: "c".into(),
            found_marker,
            ..site_a.link.clone()
        };
        Store::open(&db).unwrap().transaction(|tx| tx.set_link(&link)).unwrap();
        let site = Site {
            _dir: dir,
            root: site_a.root.clone(),
            db,
            link,
            caller: Caller {
                device_id: "c".into(),
                device_name: "c".into(),
                ..site_a.caller.clone()
            },
        };
        let mut c = site.start();
        c.connect(&remote);
        c.scan_until(|c| c.folder.scanned);
        c.rest(&mut remote);

        let case = format!("kept={kept}, found_marker={found_marker}");
        let mut on_disk = space.clone();
        on_disk.push("old.txt");
        assert_eq!(names(&site.root), on_disk, "{case}");
        match found_marker {
            true => {
                assert!(c.sent.is_empty(), "nothing is sent before an answer: {case}");
                assert_eq!(c.folder.view().questions, 1, "{case}");
                assert_eq!(remote.names(), space, "{case}");
            }
            false => {
                assert_eq!(c.folder.view().questions, 0, "{case}");
                assert_eq!(remote.names(), on_disk, "{case}");
            }
        }
    }
}

#[test]
fn a_fresh_first_link_holds_a_folder_whose_name_differs_by_case_when_the_trash_holds_its_file() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("Docs")).unwrap();
    a.create(&mut remote, &site_a.root.join("Docs/deleted.txt"), b"deleted");
    a.create(&mut remote, &site_a.root.join("Docs/keep.txt"), b"keep");
    delete_on(&mut a, &mut remote, &site_a.root.join("Docs/deleted.txt"));
    assert_eq!(remote.names(), ["Docs", "keep.txt"]);

    let site = Site::new(&remote, "b");
    fs::create_dir(site.root.join("docs")).unwrap();
    fs::write(site.root.join("docs/deleted.txt"), b"deleted").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.rest(&mut remote);

    assert_eq!(remote.names(), ["Docs", "keep.txt"]);
    assert_eq!(b.folder.view().questions, 1);
}

#[test]
fn a_fresh_first_link_adopts_a_folder_both_macs_name_in_nfd() {
    let name = "\u{1112}\u{1161}\u{11ab}";
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join(name)).unwrap();
    a.create(&mut remote, &site_a.root.join(name).join("keep.docx"), b"keep");

    let site = Site::new(&remote, "b");
    fs::create_dir(site.root.join(name)).unwrap();
    fs::write(site.root.join(name).join("keep.docx"), b"keep").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));

    assert_eq!(remote.names().len(), 2, "{:?}", remote.tree());
    assert_eq!(files(&site.root).len(), 1);
    assert_eq!(b.folder.view().questions, 0);
}

enum Before {
    Replay,
    Relist,
    Restart,
}

fn after_the_first_listing(remote: &mut Remote, site: &Site, mut b: Device, before: Before) -> Device {
    let Stream::Replaying(staging) = &b.folder.stream else {
        panic!("b replays the log onto its listing");
    };
    let since = resume(staging.history_id(), staging.last());
    match before {
        Before::Replay => b.resume(remote, since).unwrap(),
        Before::Relist => {
            remote.space.truncate_log(remote.space.head_seq());
            b.resume(remote, since).unwrap();
            assert!(matches!(b.work.front(), Some(Work::List)));
            b.run(remote);
        }
        Before::Restart => {
            drop(b);
            b = site.start();
            b.connect(remote);
            b.run(remote);
        }
    }
    b.scan_until(|b| b.folder.scanned);
    b.rest(remote);
    b
}

fn listed(remote: &mut Remote, site: &Site) -> Device {
    let mut b = site.start();
    b.connect(remote);
    b.run(remote);
    assert!(!b.folder.scanned, "the first scan has not ended");
    b
}

#[test]
fn a_file_another_device_adds_after_an_empty_listing_asks_nothing_after_a_relist_or_a_restart() {
    for before in [Before::Replay, Before::Relist, Before::Restart] {
        let mut remote = Remote::new();
        let (site_a, mut a) = linked(&mut remote, "a");
        let site = Site::new(&remote, "b");
        fs::write(site.root.join("Budget.xlsx"), b"written here").unwrap();
        let b = listed(&mut remote, &site);
        a.create(&mut remote, &site_a.root.join("Slides.pptx"), b"written on another device");
        let b = after_the_first_listing(&mut remote, &site, b, before);
        assert_eq!(shown(&site), [], "the first listing was empty");
        assert_eq!(remote.names(), ["Budget.xlsx", "Slides.pptx"]);
        assert_eq!(b.state(), FolderState::UpToDate);
    }
}

fn lost_its_store(remote: &mut Remote) -> (Site, Device, Site) {
    let (site_a, mut a) = linked(remote, "a");
    let (site, mut b) = linked(remote, "b");
    b.create(remote, &site.root.join("Budget.xlsx"), b"written on b");
    drop(b);
    fs::remove_file(&site.db).unwrap();
    let mut store = Store::open(&site.db).unwrap();
    store.transaction(|tx| tx.set_link(&site.link)).unwrap();
    a.pull(remote);
    a.rest(remote);
    (site_a, a, site)
}

fn delete_on(a: &mut Device, remote: &mut Remote, path: &Path) {
    fs::remove_file(path).unwrap();
    a.rescan();
    a.rest(remote);
}

#[test]
fn a_file_another_device_deletes_after_a_live_listing_stays_held_after_a_relist_or_a_restart() {
    for before in [Before::Replay, Before::Relist, Before::Restart] {
        let mut remote = Remote::new();
        let (site_a, mut a, site) = lost_its_store(&mut remote);
        let b = listed(&mut remote, &site);
        delete_on(&mut a, &mut remote, &site_a.root.join("Budget.xlsx"));
        let b = after_the_first_listing(&mut remote, &site, b, before);
        assert_eq!(remote.names(), Vec::<String>::new(), "the deleted file is not sent again");
        assert_eq!(b.folder.view().questions, 1);
        assert_eq!(names(&site.root), ["Budget.xlsx"]);
    }
}

#[test]
fn a_file_another_device_deleted_before_an_empty_listing_is_held() {
    for added_after in [false, true] {
        let mut remote = Remote::new();
        let (site_a, mut a, site) = lost_its_store(&mut remote);
        delete_on(&mut a, &mut remote, &site_a.root.join("Budget.xlsx"));
        let b = listed(&mut remote, &site);
        if added_after {
            a.create(&mut remote, &site_a.root.join("Slides.pptx"), b"written on another device");
        }
        let b = after_the_first_listing(&mut remote, &site, b, Before::Replay);
        let held: Vec<_> = (shown(&site).into_iter())
            .flat_map(|question| question.items.into_iter().map(|item| item.path))
            .collect();
        assert_eq!(held, ["Budget.xlsx"], "added after: {added_after}");
        assert!(!remote.names().contains(&"Budget.xlsx".to_owned()));
        assert_eq!(b.folder.view().questions, 1);
    }
}

#[test]
fn a_file_in_a_deleted_folder_another_device_recreates_after_an_empty_listing_is_held() {
    for with_a_file in [false, true] {
        let mut remote = Remote::new();
        let (site_a, mut a) = linked(&mut remote, "a");
        fs::create_dir(site_a.root.join("Docs")).unwrap();
        a.create(&mut remote, &site_a.root.join("Docs/X.docx"), b"x");
        let site = Site::new(&remote, "b");
        fs::create_dir(site.root.join("Docs")).unwrap();
        fs::write(site.root.join("Docs/X.docx"), b"x").unwrap();
        fs::remove_dir_all(site_a.root.join("Docs")).unwrap();
        a.rescan();
        a.rest(&mut remote);
        let b = listed(&mut remote, &site);
        fs::create_dir(site_a.root.join("Docs")).unwrap();
        if with_a_file {
            fs::write(site_a.root.join("Docs/New.docx"), b"new").unwrap();
        }
        a.rescan();
        a.rest(&mut remote);
        let b = after_the_first_listing(&mut remote, &site, b, Before::Replay);
        let held: Vec<_> = (shown(&site).into_iter())
            .flat_map(|question| question.items.into_iter().map(|item| item.path))
            .collect();
        assert_eq!(held, ["Docs/X.docx"], "with a file: {with_a_file}");
        assert!(!remote.names().contains(&"X.docx".to_owned()));
        assert_eq!(b.folder.view().questions, 1);
    }
}


#[test]
fn a_discard_takes_along_a_server_file_moved_into_the_held_folder_while_the_rules_left_it_out_across_a_restart()
 {
    for restart in [false, true] {
        let mut remote = Remote::new();
        let site = synced_before(&mut remote, "b");
        let (site_a, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site_a.root.join("server.txt"), b"written on a");
        a.create(&mut remote, &site_a.root.join(".koiignore"), b"left-out.txt\n");
        fs::create_dir(site.root.join("new")).unwrap();
        fs::write(site.root.join("new/mine.txt"), b"mine").unwrap();
        let mut b = site.start();
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);
        assert_eq!(b.folder.view().questions, 1, "{restart}");
        fs::rename(site.root.join("server.txt"), site.root.join("new/left-out.txt")).unwrap();
        b.scan_until(|b| {
            (b.folder.engine.trees.local().iter()).any(|(_, node)| node.shape.name == "left-out.txt")
        });
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);
        let left_out = |b: &Device| -> Vec<String> {
            (b.folder.engine.trees.local().iter())
                .filter(|(_, node)| node.left_out_when_placed)
                .map(|(_, node)| node.shape.name.clone())
                .collect()
        };
        let before = left_out(&b);
        assert_eq!(before, ["left-out.txt"], "{restart}");
        if restart {
            drop(b);
            b = site.start();
            assert_eq!(left_out(&b), before, "the flag survives the restart");
            b.connect(&remote);
            b.scan_until(|b| b.folder.scanned);
            b.run(&mut remote);
        }
        a.create(&mut remote, &site_a.root.join(".koiignore"), b"other.txt\n");
        b.pull(&remote);
        b.run(&mut remote);
        b.scan_until(|b| !b.folder.disk.awaits_rules());
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);
        assert!(b.sent.is_empty(), "{restart}: nothing is sent before the answer");

        answer(&site, Answer::Discard);
        let step = b.folder.requested();
        b.take(step).unwrap();
        for _ in 0..3 {
            b.run(&mut remote);
            b.settle();
        }
        b.run(&mut remote);
        assert!(b.sent.is_empty(), "{restart}: nothing is sent for the discarded folder");
        assert_eq!(remote.names(), [".koiignore", "server.txt"], "{restart}");
        assert_eq!(
            fs::read(site.root.join("server.txt")).unwrap(),
            b"written on a",
            "{restart}"
        );
        assert!(!site.root.join("new").exists(), "{restart}");
        assert_eq!(b.folder.view().questions, 0, "{restart}");
    }
}

#[test]
fn a_discard_takes_along_a_server_file_the_server_edits_after_the_link_across_a_restart_before_the_next_scan()
 {
    for (adopted, restart) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut remote = Remote::new();
        let site = synced_before(&mut remote, "b");
        let (site_a, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site_a.root.join("server.txt"), b"written on a");
        a.create(&mut remote, &site_a.root.join(".koiignore"), b"left-out.txt\n");
        fs::create_dir(site.root.join("new")).unwrap();
        fs::write(site.root.join("new/mine.txt"), b"mine").unwrap();
        if adopted {
            fs::write(site.root.join("server.txt"), b"written on a").unwrap();
        }
        let mut b = site.start();
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);
        assert_eq!(b.folder.view().questions, 1, "{adopted} {restart}");
        let from_server = |b: &Device| {
            (b.folder.engine.trees.local().iter())
                .any(|(_, node)| node.shape.name == "server.txt" && node.content_from_server)
        };
        assert!(from_server(&b), "{adopted} {restart}: the link holds the server's bytes");
        if restart {
            drop(b);
            b = site.start();
            b.connect(&remote);
        }
        b.scan_until(|b| b.folder.scanned);
        assert!(from_server(&b), "{adopted} {restart}: the next scan finds the server's bytes");
        fs::rename(site.root.join("server.txt"), site.root.join("new/left-out.txt")).unwrap();
        b.scan_until(|b| {
            (b.folder.engine.trees.local().iter()).any(|(_, node)| node.shape.name == "left-out.txt")
        });
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);
        a.create(&mut remote, &site_a.root.join("server.txt"), b"edited on a");
        a.create(&mut remote, &site_a.root.join(".koiignore"), b"other.txt\n");
        b.pull(&remote);
        b.run(&mut remote);
        b.scan_until(|b| !b.folder.disk.awaits_rules());
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);
        assert!(b.sent.is_empty(), "{adopted} {restart}: nothing is sent before the answer");

        answer(&site, Answer::Discard);
        let step = b.folder.requested();
        b.take(step).unwrap();
        for _ in 0..3 {
            b.run(&mut remote);
            b.settle();
        }
        b.run(&mut remote);
        assert!(b.sent.is_empty(), "{adopted} {restart}: nothing is sent for the discarded folder");
        assert_eq!(remote.names(), [".koiignore", "server.txt"], "{adopted} {restart}");
        assert_eq!(
            fs::read(site.root.join("server.txt")).unwrap(),
            b"edited on a",
            "{adopted} {restart}"
        );
        assert!(!site.root.join("new").exists(), "{adopted} {restart}");
        assert_eq!(b.folder.view().questions, 0, "{adopted} {restart}");
    }
}

#[test]
fn a_discard_takes_along_an_adopted_server_file_the_server_edits_while_the_folder_is_asked_about() {
    let mut remote = Remote::new();
    let site = synced_before(&mut remote, "b");
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("server.txt"), b"written on a");
    a.create(&mut remote, &site_a.root.join(".koiignore"), b"left-out.txt\n");
    fs::write(site.root.join("server.txt"), b"written on a").unwrap();
    fs::create_dir(site.root.join("new")).unwrap();
    fs::write(site.root.join("new/mine.txt"), b"mine").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(b.folder.view().questions, 1);
    fs::rename(site.root.join("server.txt"), site.root.join("new/left-out.txt")).unwrap();
    b.scan_until(|b| {
        (b.folder.engine.trees.local().iter()).any(|(_, node)| node.shape.name == "left-out.txt")
    });
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.create(&mut remote, &site_a.root.join("server.txt"), b"edited on a");
    a.create(&mut remote, &site_a.root.join(".koiignore"), b"other.txt\n");
    b.pull(&remote);
    b.run(&mut remote);
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert!(b.sent.is_empty(), "nothing is sent before the answer: {:?}", b.sent);

    answer(&site, Answer::Discard);
    let step = b.folder.requested();
    b.take(step).unwrap();
    for _ in 0..3 {
        b.run(&mut remote);
        b.settle();
    }
    b.run(&mut remote);
    assert!(b.sent.is_empty(), "nothing is sent for the discarded folder: {:?}", b.sent);
    assert_eq!(remote.names(), [".koiignore", "server.txt"]);
    assert_eq!(fs::read(site.root.join("server.txt")).unwrap(), b"edited on a");
    assert!(!site.root.join("new").exists());
}
#[test]
fn a_discard_takes_along_a_server_file_moved_into_an_excluded_folder_inside_the_held_folder() {
    for adopted in [false, true] {
        let mut remote = Remote::new();
        let site = synced_before(&mut remote, "b");
        let (site_a, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site_a.root.join("server.txt"), b"written on a");
        a.create(&mut remote, &site_a.root.join(".koiignore"), b"left-out/\n");
        if adopted {
            fs::write(site.root.join("server.txt"), b"written on a").unwrap();
        }
        fs::create_dir(site.root.join("new")).unwrap();
        fs::write(site.root.join("new/mine.txt"), b"mine").unwrap();
        fs::create_dir(site.root.join("new/left-out")).unwrap();
        let mut b = site.start();
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);
        assert_eq!(b.folder.view().questions, 1, "{adopted}");
        fs::rename(site.root.join("server.txt"), site.root.join("new/left-out/server.txt")).unwrap();
        for _ in 0..3 {
            b.scan_until(|b| b.folder.scanned);
            b.run(&mut remote);
            b.settle();
        }
        b.run(&mut remote);
        assert!(b.sent.is_empty(), "{adopted}: nothing is sent before the rules change");
        a.create(&mut remote, &site_a.root.join(".koiignore"), b"other.txt\n");
        b.pull(&remote);
        b.run(&mut remote);
        b.scan_until(|b| !b.folder.disk.awaits_rules());
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);
        assert!(b.sent.is_empty(), "{adopted}: nothing is sent before the answer");

        answer(&site, Answer::Discard);
        let step = b.folder.requested();
        b.take(step).unwrap();
        for _ in 0..3 {
            b.run(&mut remote);
            b.settle();
        }
        b.run(&mut remote);
        assert!(b.sent.is_empty(), "{adopted}: nothing is sent for the discarded folder: {:?}", b.sent);
        assert_eq!(remote.names(), [".koiignore", "server.txt"], "{adopted}");
        assert_eq!(fs::read(site.root.join("server.txt")).unwrap(), b"written on a", "{adopted}");
        assert!(!site.root.join("new").exists(), "{adopted}");
    }
}
