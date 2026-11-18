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



