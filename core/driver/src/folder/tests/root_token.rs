// SPDX-License-Identifier: Apache-2.0

fn opened(site: &Site) -> Result<Folder, Stop> {
    let mut link = site.link.clone();
    disk::admit_root(&mut link, &mut Store::open(&site.db).unwrap())?;
    let lock = claim::lock(&site.root)
        .unwrap()
        .expect("the folder is not locked");
    let local = local_rules(&SyncRules::default()).unwrap();
    Folder::open(link, &lock, &site.db, local)
}

fn halts_as_a_copy(site: &Site) -> bool {
    matches!(opened(site), Err(Stop::Root(RootProblem::Copy)))
}

fn marker_of(site: &Site) -> Marker {
    koidrive_fs::read_marker(&site.root).unwrap().unwrap()
}

fn tokens_of(site: &Site) -> koidrive_fs::RootTokens {
    Store::open(&site.db)
        .unwrap()
        .transaction(|tx| tx.root_tokens())
        .unwrap()
}

fn assert_rotated_from(site: &Site, earlier: &[Option<String>]) {
    let token = marker_of(site).token;
    assert!(
        token
            .as_ref()
            .is_some_and(|token| token.len() == 32 && token.chars().all(|c| c.is_ascii_hexdigit())),
        "{token:?}"
    );
    assert!(!earlier.contains(&token), "{token:?} in {earlier:?}");
    assert_eq!(
        tokens_of(site),
        koidrive_fs::RootTokens {
            current: token,
            pending: None,
        }
    );
}

fn crash_at(cut: disk::Cut) -> Box<dyn FnMut(disk::Cut) + Send> {
    Box::new(move |reached| {
        if reached == cut {
            std::panic::resume_unwind(Box::new(()));
        }
    })
}

fn block_marker(site: &Site) -> PathBuf {
    let blocker = site
        .root
        .join(koidrive_fs::PRIVATE_DIR)
        .join("space.json.tmp");
    fs::create_dir(&blocker).unwrap();
    fs::write(blocker.join("held"), b"").unwrap();
    blocker
}

fn hold(a: &mut Device, path: &Path) {
    fs::write(path, b"held").unwrap();
    a.rescan();
    a.scan_until(|a| !a.work.is_empty());
    assert_retries_the_build(a);
}

fn assert_retries_the_build(a: &Device) {
    let work: Vec<_> = a.work.iter().collect();
    assert!(
        matches!(work[..], [Work::ResendAfter(wait)] if *wait == Duration::from_secs(1)),
        "{} pieces of work",
        work.len()
    );
}

fn copy_folder(from: &Path, to: &Path) {
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn a_retry_after_a_store_recovery_stopped_at_the_probe_admits_the_folder() {
    let mut remote = Remote::new();
    let (site, a) = linked(&mut remote, "a");
    drop(a);
    let saved = site.db.with_extension("saved");
    fs::copy(&site.db, &saved).unwrap();
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    let mut a = restarted(&site, &mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["two.txt"]);
    drop(a);
    fs::copy(&saved, &site.db).unwrap();
    let marker = marker_of(&site);
    assert!(marker.effects > site.stored().effects.cleared());
    assert_ne!(tokens_of(&site).current, marker.token);
    let blocker = site
        .root
        .join(koidrive_fs::PRIVATE_DIR)
        .join("no-replace-probe-a");
    fs::create_dir(&blocker).unwrap();
    assert!(matches!(opened(&site), Err(Stop::Io(_))));
    assert_eq!(site.stored().effects.cleared(), marker.effects);
    assert_eq!(marker_of(&site), marker);
    assert_eq!(tokens_of(&site).current, marker.token);

    fs::remove_dir(&blocker).unwrap();
    assert!(opened(&site).is_ok());
    assert_rotated_from(&site, &[marker.token]);
}

#[test]
fn a_marker_write_that_failed_at_the_start_takes_the_new_token_at_the_next_effect() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    drop(a);
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    let before = marker_of(&site);
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    let blocker = site
        .root
        .join(koidrive_fs::PRIVATE_DIR)
        .join("space.json.tmp");
    fs::create_dir(&blocker).unwrap();

    let mut a = site.start();
    assert_eq!(marker_of(&site), before);
    let pending = tokens_of(&site).pending;
    assert!(pending.is_some());
    fs::remove_dir(&blocker).unwrap();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["before.txt", "two.txt"]);
    assert_eq!(marker_of(&site).token, pending);
    assert_rotated_from(&site, &[before.token]);
    drop(a);

    fs::rename(&site.root, site.root.with_file_name("original")).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    assert!(halts_as_a_copy(&site));
}

#[test]
fn a_copy_made_before_an_upload_halts_and_the_server_keeps_the_upload() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    a.create(&mut remote, &site.root.join("edited.txt"), b"old");
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    a.create(&mut remote, &site.root.join("after.txt"), b"after");
    a.create(&mut remote, &site.root.join("edited.txt"), b"new");
    drop(a);
    let served = remote.tree();

    fs::rename(&site.root, site.root.with_file_name("original")).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    assert!(halts_as_a_copy(&site));
    assert_eq!(names(&site.root), ["before.txt", "edited.txt"]);
    assert_eq!(remote.tree(), served);
}

#[test]
fn a_copy_made_before_a_build_halts_once_the_server_accepted_the_commit_whose_result_never_came() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    fs::write(site.root.join("after.txt"), b"after").unwrap();
    a.rescan();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    remote.commit(&a.caller, &commit).unwrap();
    drop(a);
    let served = remote.tree();
    assert_eq!(remote.names(), ["after.txt", "before.txt"]);

    fs::rename(&site.root, site.root.with_file_name("original")).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    assert!(halts_as_a_copy(&site));
    assert_eq!(names(&site.root), ["before.txt"]);
    assert_eq!(remote.tree(), served);
}

#[test]
fn a_copy_made_after_the_latest_upload_starts_and_loses_nothing() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    drop(a);
    let served = remote.tree();

    fs::rename(&site.root, site.root.with_file_name("original")).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    assert_eq!(names(&site.root), ["before.txt"]);
    assert_eq!(remote.tree(), served);
}

#[test]
fn a_copy_made_before_a_change_from_the_server_was_applied_does_not_start() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    a.pull(&remote);
    a.rest(&mut remote);
    assert_eq!(names(&site.root), ["before.txt", "two.txt"]);
    drop(a);
    let served = remote.tree();

    fs::rename(&site.root, site.root.with_file_name("original")).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    assert!(matches!(opened(&site), Err(Stop::DiskBehind { .. })));
    assert_eq!(names(&site.root), ["before.txt"]);
    assert_eq!(remote.tree(), served);

    let blocker = block_marker(&site);
    assert!(matches!(
        relink(&site),
        Err(disk::RelinkError::Stop(Stop::Io(_)))
    ));
    fs::remove_dir_all(&blocker).unwrap();
    assert!(matches!(relink(&site), Err(disk::RelinkError::NotACopy)));
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_eq!(remote.tree(), served);
    assert_eq!(names(&site.root), ["before.txt", "two.txt"]);
}

#[test]
fn a_crash_at_each_step_of_a_rotation_before_a_build_starts_again_without_a_halt() {
    for cut in [disk::Cut::TokenPended, disk::Cut::TokenWritten] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        let copy = site.root.with_file_name("copy");
        copy_folder(&site.root, &copy);
        let before = marker_of(&site).token;
        a.folder.disk.set_hook(crash_at(cut));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            a.create(&mut remote, &site.root.join("after.txt"), b"after")
        }));
        assert!(crashed.is_err(), "{cut:?}");
        drop(a);
        assert_eq!(remote.names(), Vec::<String>::new(), "{cut:?}");
        let tokens = tokens_of(&site);
        assert_eq!(tokens.current, before, "{cut:?}");
        let written = (cut == disk::Cut::TokenWritten).then(|| tokens.pending.clone());
        assert_eq!(marker_of(&site).token, written.unwrap_or(before.clone()), "{cut:?}");

        let mut a = restarted(&site, &mut remote);
        a.rest(&mut remote);
        assert_ne!(a.state(), FolderState::Halted, "{cut:?}");
        assert_rotated_from(&site, &[before.clone(), tokens.pending]);
        assert_eq!(remote.names(), ["after.txt"], "{cut:?}");
        a.create(&mut remote, &site.root.join("second.txt"), b"second");
        assert_eq!(remote.names(), ["after.txt", "second.txt"], "{cut:?}");
        assert_eq!(names(&site.root), ["after.txt", "second.txt"], "{cut:?}");
        drop(a);

        fs::rename(&site.root, site.root.with_file_name("original")).unwrap();
        fs::rename(&copy, &site.root).unwrap();
        assert!(halts_as_a_copy(&site), "{cut:?}");
    }
}

#[test]
fn a_crash_between_a_rotation_and_the_send_starts_again_and_sends_the_commit() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    let before = marker_of(&site).token;
    a.folder.disk.set_hook(crash_at(disk::Cut::TokenCommitted));
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        a.create(&mut remote, &site.root.join("after.txt"), b"after")
    }));
    assert!(crashed.is_err());
    drop(a);
    assert_eq!(remote.names(), Vec::<String>::new());
    let rotated = marker_of(&site).token;
    assert_ne!(rotated, before);
    assert_eq!(
        tokens_of(&site),
        koidrive_fs::RootTokens {
            current: rotated.clone(),
            pending: None,
        }
    );

    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    assert_rotated_from(&site, &[before, rotated]);
    assert_eq!(remote.names(), ["after.txt"]);
    assert_eq!(names(&site.root), ["after.txt"]);
    drop(a);

    fs::rename(&site.root, site.root.with_file_name("original")).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    assert!(halts_as_a_copy(&site));
    assert_eq!(remote.names(), ["after.txt"]);
}

#[test]
fn a_marker_write_that_fails_before_a_build_holds_the_upload_and_says_why() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let before = marker_of(&site).token;
    block_marker(&site);

    hold(&mut a, &site.root.join("after.txt"));
    let tokens = tokens_of(&site);
    assert_eq!(tokens.current, before);
    assert!(tokens.pending.is_some());
    assert_eq!(marker_of(&site).token, before);
    for _ in 0..3 {
        assert_retries_the_build(&a);
        let retry = a.work.pop_front().unwrap();
        a.perform(retry, &mut remote).unwrap();
    }
    assert_retries_the_build(&a);
    assert_eq!(remote.names(), Vec::<String>::new());
    assert_eq!(marker_of(&site).token, before);
    assert_ne!(a.state(), FolderState::Halted);
    let problem = a.folder.view().problem.unwrap_or_default();
    assert!(problem.contains("wait to be uploaded"), "{problem}");
}

#[test]
fn no_commit_goes_out_while_every_marker_write_of_the_session_fails() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    drop(a);
    let before = marker_of(&site).token;
    block_marker(&site);

    let mut a = restarted(&site, &mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    hold(&mut a, &site.root.join("one.txt"));
    let retry = a.work.pop_front().unwrap();
    a.perform(retry, &mut remote).unwrap();
    hold(&mut a, &site.root.join("two.txt"));
    let retry = a.work.pop_front().unwrap();
    a.perform(retry, &mut remote).unwrap();
    assert_retries_the_build(&a);

    assert!(a.sent.is_empty());
    assert_eq!(remote.names(), ["before.txt"]);
    assert_eq!(marker_of(&site).token, before);
    assert_eq!(tokens_of(&site).current, before);
    assert!(a.folder.engine.commits.in_flight().is_none());
    assert_eq!(names(&site.root), ["before.txt", "one.txt", "two.txt"]);
}

#[test]
fn an_upload_held_by_a_failed_rotation_is_sent_once_a_later_rotation_succeeds() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    let before = marker_of(&site).token;
    let blocker = block_marker(&site);
    hold(&mut a, &site.root.join("after.txt"));
    assert_eq!(remote.names(), ["before.txt"]);

    fs::remove_dir_all(&blocker).unwrap();
    a.run(&mut remote);
    assert_eq!(remote.names(), ["after.txt", "before.txt"]);
    assert_eq!(a.folder.view().problem, None);
    assert_rotated_from(&site, &[before]);
    drop(a);

    fs::rename(&site.root, site.root.with_file_name("original")).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    assert!(halts_as_a_copy(&site));
    assert_eq!(remote.names(), ["after.txt", "before.txt"]);
}

#[test]
fn an_upload_waits_for_a_marker_write_whose_flush_succeeded() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    let before = marker_of(&site).token;
    a.folder.disk.fail_marker_flush(true);

    hold(&mut a, &site.root.join("after.txt"));
    let tokens = tokens_of(&site);
    assert_eq!(tokens.current, before);
    assert!(tokens.pending.is_some());
    assert_eq!(marker_of(&site).token, tokens.pending);
    for _ in 0..2 {
        let retry = a.work.pop_front().unwrap();
        a.perform(retry, &mut remote).unwrap();
        assert_retries_the_build(&a);
        assert_eq!(remote.names(), ["before.txt"]);
        assert_eq!(tokens_of(&site).current, before);
    }

    a.folder.disk.fail_marker_flush(false);
    a.run(&mut remote);
    assert_eq!(remote.names(), ["after.txt", "before.txt"]);
    assert_rotated_from(&site, &[before]);
}

#[test]
fn a_copy_restored_after_a_failed_rotation_loses_nothing_and_the_server_keeps_its_files() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    let before = marker_of(&site);
    block_marker(&site);
    hold(&mut a, &site.root.join("after.txt"));
    assert_eq!(marker_of(&site), before);
    assert_eq!(remote.names(), ["before.txt"]);
    drop(a);

    let original = site.root.with_file_name("original");
    fs::rename(&site.root, &original).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    if !halts_as_a_copy(&site) {
        let mut a = restarted(&site, &mut remote);
        a.rest(&mut remote);
        assert_eq!(names(&site.root), ["before.txt"]);
    }
    assert_eq!(remote.names(), ["before.txt"]);
    assert_eq!(names(&original), ["after.txt", "before.txt"]);
}

#[test]
fn a_copy_of_the_folder_made_before_its_last_start_halts_as_a_copy() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    drop(a);
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    let mut a = restarted(&site, &mut remote);
    a.create(&mut remote, &site.root.join("after.txt"), b"after");
    drop(a);

    let original = site.root.with_file_name("original");
    fs::rename(&site.root, &original).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    let copied = marker_of(&site);
    assert!(halts_as_a_copy(&site));
    assert_eq!(marker_of(&site), copied);
    assert_eq!(names(&site.root), ["before.txt"]);
    assert_eq!(remote.names(), ["after.txt", "before.txt"]);

    fs::rename(&site.root, &copy).unwrap();
    fs::rename(&original, &site.root).unwrap();
    let a = restarted(&site, &mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    assert_eq!(names(&site.root), ["after.txt", "before.txt"]);
    assert_eq!(remote.names(), ["after.txt", "before.txt"]);
}

#[test]
fn a_marker_without_a_token_takes_one_at_its_first_start_and_another_at_each_start() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("kept.txt"), b"kept");
    drop(a);
    let effects = marker_of(&site).effects;
    fs::write(
        site.root.join(koidrive_fs::PRIVATE_DIR).join("space.json"),
        format!(r#"{{"space_id":"{}","effects":{effects}}}"#, site.link.space_id),
    )
    .unwrap();
    let mut store = Store::open(&site.db).unwrap();
    store
        .transaction(|tx| tx.set_root_tokens(&koidrive_fs::RootTokens::default()))
        .unwrap();
    drop(store);

    let a = restarted(&site, &mut remote);
    assert_rotated_from(&site, &[None]);
    assert_ne!(a.state(), FolderState::Halted);
    assert_eq!(names(&site.root), ["kept.txt"]);
    assert_eq!(remote.names(), ["kept.txt"]);
    drop(a);
    let first = marker_of(&site).token;
    drop(site.start());
    assert_rotated_from(&site, &[None, first]);
}

#[test]
fn a_crash_at_each_step_of_a_rotation_starts_again_without_a_halt() {
    for upgraded in [false, true] {
        for written in [false, true] {
            let context = format!("upgraded {upgraded}, marker written {written}");
            let mut remote = Remote::new();
            let (site, a) = linked(&mut remote, "a");
            drop(a);
            let mut marker = marker_of(&site);
            if upgraded {
                marker.token = None;
                write_marker(&site.root, &marker).unwrap();
            }
            let mut store = Store::open(&site.db).unwrap();
            let pending = disk::pend_token(&marker, &mut store).unwrap();
            if written {
                let rotated = Marker {
                    token: Some(pending.clone()),
                    ..marker.clone()
                };
                write_marker(&site.root, &rotated).unwrap();
            }
            drop(store);

            let mut a = restarted(&site, &mut remote);
            assert_rotated_from(&site, &[marker.token.clone(), Some(pending)]);
            a.create(&mut remote, &site.root.join("after.txt"), b"after");
            assert_eq!(remote.names(), ["after.txt"], "{context}");
        }
    }
}

#[test]
fn a_marker_the_start_cannot_write_keeps_its_token_until_a_start_can() {
    let mut remote = Remote::new();
    let (site, a) = linked(&mut remote, "a");
    drop(a);
    let before = marker_of(&site).token;
    let blocker = site
        .root
        .join(koidrive_fs::PRIVATE_DIR)
        .join("space.json.tmp");
    fs::create_dir(&blocker).unwrap();
    fs::write(blocker.join("held"), b"").unwrap();

    let a = restarted(&site, &mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    assert_eq!(marker_of(&site).token, before);
    let tokens = tokens_of(&site);
    assert_eq!(tokens.current, before);
    drop(a);
    drop(site.start());
    assert_eq!(marker_of(&site).token, before);

    fs::remove_dir_all(&blocker).unwrap();
    drop(site.start());
    assert_rotated_from(&site, &[before, tokens.pending]);
}

#[test]
fn a_store_older_than_the_folder_recovers_whatever_token_the_folder_carries() {
    let mut remote = Remote::new();
    let (site, a) = linked(&mut remote, "a");
    drop(a);
    let saved = site.db.with_extension("saved");
    fs::copy(&site.db, &saved).unwrap();
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    let mut a = restarted(&site, &mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["two.txt"]);
    drop(a);
    fs::copy(&saved, &site.db).unwrap();
    assert_ne!(tokens_of(&site).current, marker_of(&site).token);

    let a = site.start();
    assert!(matches!(
        a.folder.engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));
    assert_rotated_from(&site, &[]);
}

#[test]
fn a_store_restored_from_before_130_starts_recovers_a_folder_with_more_effects() {
    let mut remote = Remote::new();
    let (site, a) = linked(&mut remote, "a");
    drop(a);
    let saved = site.db.with_extension("saved");
    fs::copy(&site.db, &saved).unwrap();
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    let mut a = restarted(&site, &mut remote);
    a.settle();
    a.run(&mut remote);
    drop(a);
    for _ in 0..130 {
        drop(site.start());
    }
    fs::copy(&saved, &site.db).unwrap();

    let mut a = site.start();
    assert!(matches!(
        a.folder.engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));
    a.connect(&remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["two.txt"]);
    assert_eq!(remote.names(), ["two.txt"]);
    assert!(a.folder.engine.commits.halted().is_none());
}

fn recovers(site: &Site) -> bool {
    opened(site).is_ok_and(|folder| folder.engine.commits.halted().is_some())
}

fn store_clone(site: &Site) -> Site {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.sqlite");
    fs::copy(&site.db, &db).unwrap();
    Site {
        _dir: dir,
        root: site.root.clone(),
        db,
        link: site.link.clone(),
        caller: site.caller.clone(),
    }
}

fn recoveries_while_taking_turns(sites: [&Site; 2], remote: &mut Remote) -> usize {
    let mut recovered = 0;
    for _ in 0..3 {
        for site in sites {
            recovered += usize::from(recovers(site));
            if !halts_as_a_copy(site) {
                drop(restarted(site, remote));
            }
        }
    }
    recovered
}

#[test]
fn two_clones_of_one_store_taking_turns_on_one_folder_never_recover() {
    let mut remote = Remote::new();
    let (site, a) = linked(&mut remote, "a");
    drop(a);
    let clone = store_clone(&site);
    let recovered = recoveries_while_taking_turns([&site, &clone], &mut remote);
    assert_eq!(recovered, 0);
}

fn rotated_by_a_link_without_a_token_after_its_first_marker_write_failed(
    remote: &mut Remote,
) -> (Site, Site) {
    let (site, mut a) = linked(remote, "a");
    a.create(remote, &site.root.join("kept.txt"), b"kept");
    drop(a);
    for _ in 0..2 {
        drop(restarted(&site, remote));
    }
    let other = store_clone(&site);
    let mut store = Store::open(&other.db).unwrap();
    let none = koidrive_fs::RootTokens::default();
    store.transaction(|tx| tx.set_root_tokens(&none)).unwrap();
    drop(store);
    let blocker = block_marker(&other);
    drop(opened(&other).unwrap());
    fs::remove_dir_all(&blocker).unwrap();
    drop(opened(&other).unwrap());
    assert_eq!(marker_of(&site).effects, site.stored().effects.cleared());
    (site, other)
}

#[test]
fn a_link_finds_its_folder_rotated_by_a_link_without_a_token_after_a_failed_write_and_halts() {
    let mut remote = Remote::new();
    let (site, _) =
        rotated_by_a_link_without_a_token_after_its_first_marker_write_failed(&mut remote);
    assert!(halts_as_a_copy(&site));
}

#[test]
fn two_links_taking_turns_after_a_failed_first_marker_write_never_recover_and_keep_files() {
    let mut remote = Remote::new();
    let (site, other) =
        rotated_by_a_link_without_a_token_after_its_first_marker_write_failed(&mut remote);
    let recovered = recoveries_while_taking_turns([&site, &other], &mut remote);
    assert_eq!(recovered, 0);
    assert_eq!(names(&site.root), ["kept.txt"]);
    assert_eq!(remote.names(), ["kept.txt"]);
}

fn restored_from_before_a_download(remote: &mut Remote) -> Site {
    let (site, a) = linked(remote, "a");
    drop(a);
    let saved = site.db.with_extension("saved");
    fs::copy(&site.db, &saved).unwrap();
    let (other, mut b) = linked(remote, "b");
    b.create(remote, &other.root.join("two.txt"), b"two");
    let mut a = restarted(&site, remote);
    a.settle();
    a.run(remote);
    assert_eq!(names(&site.root), ["two.txt"]);
    drop(a);
    fs::copy(&saved, &site.db).unwrap();
    site
}

#[test]
fn a_marker_write_that_fails_after_a_store_older_recovery_leaves_the_next_start_current() {
    let mut remote = Remote::new();
    let site = restored_from_before_a_download(&mut remote);
    let blocker = block_marker(&site);
    let mut a = site.start();
    assert!(a.folder.engine.commits.halted().is_some());
    a.connect(&remote);
    a.settle();
    a.run(&mut remote);
    assert!(a.folder.engine.commits.halted().is_none());
    drop(a);
    fs::remove_dir_all(&blocker).unwrap();
    assert!(!recovers(&site));
    assert_eq!(names(&site.root), ["two.txt"]);
}

#[test]
fn a_store_from_before_tokens_restored_from_before_a_download_recovers_at_its_first_start() {
    let mut remote = Remote::new();
    let site = restored_from_before_a_download(&mut remote);
    let marker = marker_of(&site);
    fs::write(
        marker_file(&site),
        format!(
            r#"{{"space_id":"{}","effects":{}}}"#,
            marker.space_id, marker.effects
        ),
    )
    .unwrap();
    rusqlite::Connection::open(&site.db)
        .unwrap()
        .execute_batch(
            "ALTER TABLE link DROP COLUMN root_token;
             ALTER TABLE link DROP COLUMN root_token_pending;
             DELETE FROM migrations WHERE name = 'root_token';",
        )
        .unwrap();

    let mut a = site.start();
    assert!(matches!(
        a.folder.engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));
    assert_rotated_from(&site, &[None]);
    a.connect(&remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["two.txt"]);
    assert_eq!(remote.names(), ["two.txt"]);
    assert!(a.folder.engine.commits.halted().is_none());
}

fn marker_file(site: &Site) -> PathBuf {
    site.root.join(koidrive_fs::PRIVATE_DIR).join("space.json")
}

fn as_an_older_build_rewrote_it(site: &Site) {
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(marker_file(site)).unwrap()).unwrap();
    let kept: serde_json::Map<String, serde_json::Value> = (json.as_object().unwrap().iter())
        .filter(|(key, _)| ["space_id", "effects", "token"].contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    fs::write(marker_file(site), serde_json::to_vec(&kept).unwrap()).unwrap();
}

#[test]
fn a_copy_from_before_a_downgrade_and_reupgrade_halts_as_a_copy() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("kept.txt"), b"kept");
    a.create(&mut remote, &site.root.join("edited.txt"), b"old");
    drop(a);
    for _ in 0..3 {
        drop(restarted(&site, &mut remote));
    }
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    as_an_older_build_rewrote_it(&site);
    let mut a = restarted(&site, &mut remote);
    a.create(&mut remote, &site.root.join("after.txt"), b"after");
    a.create(&mut remote, &site.root.join("edited.txt"), b"new");
    drop(a);
    fs::rename(&site.root, site.root.with_file_name("original")).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    assert!(halts_as_a_copy(&site));
}

#[test]
fn a_copy_another_link_carried_recovers_above_the_store_count_and_halts_at_it() {
    for equal in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site.root.join("kept.txt"), b"kept");
        drop(a);
        let (other, mut b) = linked(&mut remote, "b");
        if equal {
            b.create(&mut remote, &other.root.join("theirs.txt"), b"theirs");
            let mut a = restarted(&site, &mut remote);
            a.settle();
            a.run(&mut remote);
        }
        drop(b);
        let copy = site.root.with_file_name("copy");
        copy_folder(&site.root, &copy);
        for _ in 0..8 {
            drop(restarted(&other, &mut remote));
        }
        fs::copy(
            marker_file(&other),
            copy.join(koidrive_fs::PRIVATE_DIR).join("space.json"),
        )
        .unwrap();
        let mut a = restarted(&site, &mut remote);
        a.create(&mut remote, &site.root.join("after.txt"), b"after");
        drop(a);
        let counted = marker_of(&other).effects;
        assert_eq!(counted == site.stored().effects.cleared(), equal);
        fs::rename(&site.root, site.root.with_file_name("original")).unwrap();
        fs::rename(&copy, &site.root).unwrap();
        if equal {
            assert!(halts_as_a_copy(&site));
        } else {
            assert!(recovers(&site));
        }
    }
}

#[test]
fn a_marker_two_links_share_halts_whichever_finds_the_others_and_writes_nothing() {
    let mut remote = Remote::new();
    let (first, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &first.root.join("one.txt"), b"one");
    drop(a);
    let (second, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &second.root.join("two.txt"), b"two");
    drop(b);
    let mut a = restarted(&first, &mut remote);
    a.settle();
    a.run(&mut remote);
    drop(a);
    for _ in 0..8 {
        drop(restarted(&second, &mut remote));
    }
    assert_eq!(
        first.stored().effects.cleared(),
        second.stored().effects.cleared()
    );
    for _ in 0..2 {
        for (from, to) in [(&second, &first), (&first, &second)] {
            let own = fs::read(marker_file(to)).unwrap();
            fs::copy(marker_file(from), marker_file(to)).unwrap();
            let before = (fs::read(marker_file(to)).unwrap(), tokens_of(to));
            assert!(halts_as_a_copy(to));
            assert_eq!((fs::read(marker_file(to)).unwrap(), tokens_of(to)), before);
            fs::write(marker_file(to), own).unwrap();
            drop(restarted(to, &mut remote));
        }
    }
}

#[test]
fn two_copies_of_one_disk_halt_when_the_older_one_is_at_the_folder() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let mut remote = Remote::new();
    let site = Site::new(&remote, "a");
    let image = disks.image("linked");
    disks.attach(&image, &site.root);
    let marker = Marker {
        space_id: site.link.space_id.clone(),
        effects: 0,
        token: None,
    };
    write_marker(&site.root, &marker).unwrap();
    let found = found_root(&site.root);
    let link = Link {
        root_dev: found.dev,
        root_volume: found.volume,
        ..site.link.clone()
    };
    let mut store = Store::open(&site.db).unwrap();
    store.transaction(|tx| tx.set_link(&link)).unwrap();
    drop(store);
    let mut a = restarted(&site, &mut remote);
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    drop(a);
    disks.detach(&site.root);
    let copy = disks.copy(&image, "copy");

    disks.attach(&image, &site.root);
    let mut a = restarted(&site, &mut remote);
    a.create(&mut remote, &site.root.join("after.txt"), b"after");
    drop(a);
    disks.detach(&site.root);

    disks.attach(&copy, &site.root);
    assert_eq!(found_root(&site.root).volume, found.volume);
    assert!(halts_as_a_copy(&site));
    assert!(site.root.join("before.txt").exists());
    assert!(!site.root.join("after.txt").exists());
    assert!(remote.tree().contains_key("after.txt"));
    disks.detach(&site.root);

    disks.attach(&image, &site.root);
    let a = restarted(&site, &mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    assert!(remote.tree().contains_key("after.txt"));
    drop(a);
    disks.detach(&site.root);
}

fn relink(site: &Site) -> Result<u64, disk::RelinkError> {
    relink_until(site, Box::new(|_| {}))
}

fn relink_until(
    site: &Site,
    hook: Box<dyn FnMut(disk::Cut) + Send>,
) -> Result<u64, disk::RelinkError> {
    let lock = claim::lock(&site.root)
        .unwrap()
        .expect("the folder is not locked");
    disk::relink_with_hook(&lock, &site.link, &mut Store::open(&site.db).unwrap(), hook)
}

fn a_copy_from_before_an_add_an_edit_and_a_delete(remote: &mut Remote) -> Site {
    let (site, mut a) = linked(remote, "a");
    a.create(remote, &site.root.join("kept.txt"), b"kept");
    a.create(remote, &site.root.join("edited.txt"), b"old");
    a.create(remote, &site.root.join("gone.txt"), b"gone");
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    a.create(remote, &site.root.join("after.txt"), b"after");
    a.create(remote, &site.root.join("edited.txt"), b"new");
    fs::remove_file(site.root.join("gone.txt")).unwrap();
    a.rescan();
    a.rest(remote);
    drop(a);
    assert_eq!(remote.names(), ["after.txt", "edited.txt", "kept.txt"]);
    fs::rename(&site.root, site.root.with_file_name("original")).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    assert!(halts_as_a_copy(&site));
    site
}

#[test]
fn a_relinked_copy_loses_nothing_and_its_older_files_give_way_to_the_servers_once_discarded() {
    let mut remote = Remote::new();
    let site = a_copy_from_before_an_add_an_edit_and_a_delete(&mut remote);
    let served = remote.tree();
    let copied = marker_of(&site).token;
    let stored = tokens_of(&site).current;
    let private = site.root.join(koidrive_fs::PRIVATE_DIR);
    for dir in [
        koidrive_fs::STAGING_DIR,
        koidrive_fs::QUARANTINE_DIR,
        koidrive_fs::TRASH_DIR,
    ] {
        fs::create_dir_all(private.join(dir)).unwrap();
        fs::write(private.join(dir).join("left.txt"), b"left").unwrap();
    }

    relink(&site).unwrap();
    assert_rotated_from(&site, &[copied, stored]);
    assert!(files(&private.join(koidrive_fs::QUARANTINE_DIR)).is_empty());
    let back = (names(&site.root).into_iter())
        .find(|name| name.starts_with("left (recovered "))
        .expect("the quarantined file is back");
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    assert_eq!(remote.tree(), served);
    let file = |name: &str, text: &str| (name.to_owned(), text.to_owned());
    assert_eq!(
        files(&site.root),
        [
            file("after.txt", "after"),
            file("edited.txt", "old"),
            file("gone.txt", "gone"),
            file("kept.txt", "kept"),
            file(&back, "left"),
        ]
    );
    let [question] = &shown(&site)[..] else {
        panic!("one question");
    };
    let held: Vec<_> = (question.items.iter())
        .map(|item| item.path.unquoted())
        .collect();
    assert_eq!(held, ["edited.txt", "gone.txt", back.as_str()]);
    drop(a);

    answer(&site, Answer::Discard);
    let mut a = restarted(&site, &mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(a.folder.view().questions, 0);
    assert_eq!(remote.tree(), served);
    assert_eq!(
        files(&site.root),
        [
            file("after.txt", "after"),
            file("edited.txt", "new"),
            file("kept.txt", "kept"),
        ]
    );
    let mut trashed: Vec<_> = (files(&private.join(koidrive_fs::TRASH_DIR)).into_iter())
        .map(|(_, text)| text)
        .filter(|text| !text.starts_with('{'))
        .collect();
    trashed.sort();
    assert_eq!(trashed, ["gone", "left", "left", "old"]);
}

#[test]
fn a_file_the_newer_copy_deleted_reaches_the_server_only_once_the_question_is_answered() {
    let mut remote = Remote::new();
    let site = a_copy_from_before_an_add_an_edit_and_a_delete(&mut remote);
    relink(&site).unwrap();
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    drop(a);

    answer(&site, Answer::Publish);
    let mut a = restarted(&site, &mut remote);
    a.settle();
    a.run(&mut remote);
    assert!(remote.names().contains(&"gone.txt".to_owned()));
    assert_eq!(a.folder.view().questions, 0);
}

#[test]
fn relink_refuses_a_folder_that_passes_its_check_and_takes_one_whose_store_was_restored_on_its_own()
{
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    a.create(&mut remote, &site.root.join("notes.txt"), b"notes");
    let node = node_named(&a, "notes.txt");
    fs::remove_file(site.root.join("notes.txt")).unwrap();
    a.rescan();
    a.rest(&mut remote);
    drop(a);
    let (marker, tokens) = (marker_of(&site), tokens_of(&site));
    assert!(matches!(relink(&site), Err(disk::RelinkError::NotACopy)));
    assert_eq!((marker_of(&site), tokens_of(&site)), (marker, tokens));
    assert!(site.stored().cursor.is_some());
    let saved = site.db.with_extension("saved");
    fs::copy(&site.db, &saved).unwrap();
    drop(restarted(&site, &mut remote));
    fs::copy(&saved, &site.db).unwrap();
    assert!(halts_as_a_copy(&site));
    let id = ask(&site, &RestoreIntent::from_trash(node, None, None).unwrap());

    relink(&site).unwrap();
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    assert_eq!(a.folder.view().questions, 0);
    assert_eq!(ended(&site), [(id, Outcome::Restored)]);
    a.create(&mut remote, &site.root.join("two.txt"), b"two");
    assert_eq!(remote.names(), ["notes.txt", "one.txt", "two.txt"]);
    assert_eq!(names(&site.root), ["notes.txt", "one.txt", "two.txt"]);
}

#[test]
fn relink_checks_the_folder_against_the_disk_identity_the_store_holds() {
    let mut remote = Remote::new();
    let site = a_copy_from_before_an_add_an_edit_and_a_delete(&mut remote);
    let stale = Link {
        root_dev: site.link.root_dev + 1,
        root_volume: None,
        ..site.link.clone()
    };
    let lock = claim::lock(&site.root).unwrap().unwrap();
    disk::relink(&lock, &stale, &mut Store::open(&site.db).unwrap()).unwrap();
}

#[test]
fn a_relinked_copy_of_a_device_that_only_downloaded_holds_its_own_new_files() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("kept.txt"), b"kept");
    let (other, mut b) = linked(&mut remote, "b");
    b.pull(&remote);
    b.rest(&mut remote);
    drop(b);
    let copy = other.root.with_file_name("copy");
    copy_folder(&other.root, &copy);
    drop(other.start());
    fs::rename(&other.root, other.root.with_file_name("original")).unwrap();
    fs::rename(&copy, &other.root).unwrap();
    fs::write(other.root.join("mine.txt"), b"mine").unwrap();
    assert!(halts_as_a_copy(&other));

    relink(&other).unwrap();
    let mut b = restarted(&other, &mut remote);
    b.rest(&mut remote);
    assert_eq!(remote.names(), ["kept.txt"]);
    assert_eq!(b.folder.view().questions, 1);
}

#[test]
fn a_relink_holds_a_version_restore_whose_result_was_lost_until_the_person_answers() {
    for retried in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        let path = site.root.join("notes.txt");
        a.create(&mut remote, &path, b"the first words");
        fs::write(&path, b"and the second").unwrap();
        a.scan_until(|a| !a.work.is_empty());
        a.run(&mut remote);
        let node = node_named(&a, "notes.txt");
        let request = ListVersions {
            node_id: node.clone(),
            ..Default::default()
        };
        let first = remote.space.list_versions(&request).unwrap().versions[1]
            .version_id
            .clone();
        let intent = RestoreIntent::version(node, first).unwrap();
        let id = ask(&site, &intent);
        let step = a.folder.requested();
        a.take(step).unwrap();
        let Some(Work::Send(commit)) = a.work.pop_front() else {
            panic!("the restore is sent");
        };
        let sent: Vec<Option<op::Op>> = commit.ops.iter().map(|op| op.op.clone()).collect();
        assert_eq!(sent, [Some(op::Op::Restore(intent.op().clone()))]);
        let applied = koidrive_proto::v1::op_result::Code::Applied as i32;
        assert_eq!(
            remote.commit(&a.caller, &commit).unwrap().results[0].code,
            applied
        );
        drop(a);
        let head = remote.space.head_seq();
        let mut marker = marker_of(&site);
        marker.token = Some("0".repeat(32));
        koidrive_fs::write_marker(&site.root, &marker).unwrap();

        let blocked = site.root.join(".koidrive/space.json.tmp");
        if retried {
            let blocking = blocked.clone();
            let cut = move |cut| {
                if cut == disk::Cut::TokenPended {
                    fs::create_dir(&blocking).unwrap();
                }
            };
            assert!(relink_until(&site, Box::new(cut)).is_err());
            assert!(halts_as_a_copy(&site));
        } else {
            assert_eq!(relink(&site).unwrap(), 1);
            let mut a = restarted(&site, &mut remote);
            a.rest(&mut remote);
            assert_eq!(remote.space.head_seq(), head);
            assert!(a.folder.engine.commits.restores().contains_key(&id));
            drop(a);
        }

        let held = (shown(&site).into_iter())
            .find(|question| question.items.iter().any(|item| item.node_id == id))
            .expect("a question holds the restore");
        let given = koidrive_store::GivenAnswer {
            answer: Answer::Cancel.as_str().into(),
            answered_at_ms: NOW,
        };
        (Store::open(&site.db).unwrap())
            .transaction(|tx| tx.answer_question(&held.id, &given))
            .unwrap();
        if retried {
            fs::remove_dir(&blocked).unwrap();
            assert_eq!(relink(&site).unwrap(), 0);
        }
        let mut a = restarted(&site, &mut remote);
        a.rest(&mut remote);
        assert_eq!(remote.space.head_seq(), head);
        let cancelled = Outcome::Rejected(koidrive_engine::restore::Rejection::Cancelled);
        assert_eq!(
            ended(&site),
            [(id.clone(), cancelled)],
            "retried: {retried}"
        );
        let taken = (Store::open(&site.db).unwrap())
            .transaction(|tx| tx.taken_question(&held.id))
            .unwrap();
        assert_eq!(taken.and_then(|taken| taken.answer), Some(given));
        assert!(
            shown(&site)
                .iter()
                .all(|question| question.items.iter().all(|item| item.node_id != id))
        );
    }
}

#[test]
fn a_crash_at_each_step_of_a_relink_leaves_the_copy_halted_or_relinked() {
    for cut in [
        disk::Cut::TokenPended,
        disk::Cut::TokenWritten,
        disk::Cut::TokenCommitted,
    ] {
        let mut remote = Remote::new();
        let site = a_copy_from_before_an_add_an_edit_and_a_delete(&mut remote);
        let served = remote.tree();
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            relink_until(&site, crash_at(cut))
        }));
        assert!(crashed.is_err(), "{cut:?}");
        if cut == disk::Cut::TokenPended {
            assert!(halts_as_a_copy(&site), "{cut:?}");
            relink(&site).unwrap();
        }

        let mut a = restarted(&site, &mut remote);
        a.rest(&mut remote);
        assert_ne!(a.state(), FolderState::Halted, "{cut:?}");
        assert_eq!(remote.tree(), served, "{cut:?}");
        assert!(
            files(&site.root).contains(&("after.txt".into(), "after".into())),
            "{cut:?}"
        );
    }
}

#[test]
fn a_relink_of_a_moved_folder_or_one_whose_marker_write_fails_keeps_the_folder_halted() {
    let mut remote = Remote::new();
    let site = a_copy_from_before_an_add_an_edit_and_a_delete(&mut remote);
    let served = remote.tree();
    let (marker, tokens) = (marker_of(&site), tokens_of(&site));
    let lock = claim::lock(&site.root).unwrap().unwrap();
    let parent = site.root.parent().unwrap();
    let moved = parent.with_extension("moved");
    fs::rename(parent, &moved).unwrap();
    let mut store = Store::open(moved.join(site.db.file_name().unwrap())).unwrap();
    let refused = disk::relink(&lock, &site.link, &mut store);
    drop((store, lock));
    fs::rename(&moved, parent).unwrap();
    assert!(matches!(
        refused,
        Err(disk::RelinkError::Stop(Stop::Withdrawn(_)))
    ));
    assert_eq!((marker_of(&site), tokens_of(&site)), (marker, tokens));
    let blocker = block_marker(&site);

    assert!(matches!(
        relink(&site),
        Err(disk::RelinkError::Stop(Stop::Io(_)))
    ));
    assert!(halts_as_a_copy(&site));
    fs::remove_dir_all(&blocker).unwrap();
    assert!(halts_as_a_copy(&site));

    relink(&site).unwrap();
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    assert_eq!(remote.tree(), served);
}

#[test]
fn a_link_that_adopted_a_token_and_failed_its_write_does_not_hand_it_back() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    a.pull(&remote);
    a.rest(&mut remote);
    drop(a);
    drop(b);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.sqlite");
    let mut link = site.link.clone();
    link.device_id = "c".into();
    Store::open(&db)
        .unwrap()
        .transaction(|tx| tx.set_link(&link))
        .unwrap();
    let mut caller = site.caller.clone();
    caller.device_id = "c".into();
    caller.device_name = "c".into();
    let second = Site {
        _dir: dir,
        root: site.root.clone(),
        db,
        link,
        caller,
    };
    let blocker = block_marker(&second);
    drop(opened(&second).unwrap());
    fs::remove_dir_all(&blocker).unwrap();
    drop(opened(&second).unwrap());
    assert!(halts_as_a_copy(&site));
}
