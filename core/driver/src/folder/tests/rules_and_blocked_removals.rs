// SPDX-License-Identifier: Apache-2.0

#[test]
fn the_device_list_narrows_what_is_downloaded_and_what_is_uploaded() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"notes");
    a.create(&mut remote, &site_a.root.join("debug.log"), b"log of a");

    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["*.log".into()],
    };
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage"]);
    assert_eq!(names(&site.root), ["notes.txt"]);

    fs::write(site.root.join("mine.log"), b"log of b").unwrap();
    b.settle();
    assert!(b.work.is_empty() && b.sent.is_empty());
    assert_eq!(remote.names(), ["debug.log", "notes.txt"]);
    assert_eq!(
        b.state(),
        FolderState::UpToDate,
        "what the device leaves out does not keep the folder catching up"
    );
}

#[test]
fn a_downloaded_rules_file_is_in_force_before_anything_else_is_planned() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("cache.tmp"), b"cache");
    a.create(&mut remote, &site_a.root.join(".koiignore"), b"*.tmp\n");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage"]);
    assert_eq!(names(&site.root), [".koiignore"]);
    assert!(b.folder.disk.awaits_rules());
    assert_eq!(b.state(), FolderState::CatchingUp);

    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(
        b.run(&mut remote).is_empty(),
        "the rules leave cache.tmp out"
    );
    b.settle();
    assert!(b.work.is_empty() && b.sent.is_empty());
    assert_eq!(names(&site.root), [".koiignore"]);
    assert_eq!(remote.names(), [".koiignore", "cache.tmp"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

/// After a restart the scan cannot read a rules file over the size limit, and no earlier scan of this run read it, so
/// what it leaves out is unknown. Nothing below its folder is planned: the excluded log keeps this device's bytes, and
/// the new file waits too. Once the file can be read, the log stays out and the new file arrives.
#[test]
fn a_rules_file_a_restart_cannot_read_holds_everything_below_its_folder() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("build.log"), b"v1");
    let (site, mut b) = linked(&mut remote, "b");
    let (site_c, mut c) = linked(&mut remote, "c");
    a.create(&mut remote, &site_a.root.join(".koiignore"), b"*.log\n");
    b.pull(&remote);
    assert_eq!(b.run(&mut remote), ["stage"]);
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());
    drop(b);
    fs::write(site.root.join(".koiignore"), "*.log\n".repeat(200_000)).unwrap();
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"notes");
    c.create(&mut remote, &site_c.root.join("build.log"), b"v2 from c");

    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert_eq!(b.run(&mut remote), ["subscribe"]);
    assert_eq!(fs::read(site.root.join("build.log")).unwrap(), b"v1");
    assert_eq!(names(&site.root), [".koiignore", "build.log"]);

    fs::write(site.root.join(".koiignore"), "*.log\n").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    assert_eq!(b.run(&mut remote), ["stage"]);
    b.settle();
    assert!(b.run(&mut remote).is_empty());
    assert_eq!(names(&site.root), [".koiignore", "build.log", "notes.txt"]);
    assert_eq!(fs::read(site.root.join("build.log")).unwrap(), b"v1");
    assert!(b.work.is_empty() && b.sent.is_empty());
}

/// A restart cannot read the rules file of `proj`, so what is below it is held. The user renames the folder to `PROJ`
/// and fixes the file, and the rules are known again under the new spelling. Renamed back, the folder is not held:
/// the hold is by `name_key`, as the rules are, so a later remote edit arrives.
#[test]
fn the_rules_hold_of_a_folder_ends_also_after_it_was_renamed_only_in_case() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("proj")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/file.txt"), b"v1");
    a.create(
        &mut remote,
        &site_a.root.join("proj/.koiignore"),
        b"*.log\n",
    );
    let (site, mut b) = linked(&mut remote, "b");
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    b.run(&mut remote);
    drop(b);
    fs::write(site.root.join("proj/.koiignore"), "*.log\n".repeat(200_000)).unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    fs::rename(site.root.join("proj"), site.root.join("PROJ")).unwrap();
    fs::write(site.root.join("PROJ/.koiignore"), b"*.log\n").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    fs::rename(site.root.join("PROJ"), site.root.join("proj")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.create(
        &mut remote,
        &site_a.root.join("proj/file.txt"),
        b"v2 from a",
    );
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        fs::read(site.root.join("proj/file.txt")).unwrap(),
        b"v2 from a"
    );
}

#[test]
fn review_removing_unknown_rules_while_renaming_the_folder_does_not_hold_remote_updates_forever() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("proj")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/file.txt"), b"v1");
    a.create(
        &mut remote,
        &site_a.root.join("proj/.koiignore"),
        b"*.log\n",
    );
    let (site, mut b) = linked(&mut remote, "b");
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    b.run(&mut remote);
    drop(b);
    fs::write(site.root.join("proj/.koiignore"), "*.log\n".repeat(200_000)).unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    fs::rename(site.root.join("proj"), site.root.join("PROJ")).unwrap();
    fs::remove_file(site.root.join("PROJ/.koiignore")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    fs::rename(site.root.join("PROJ"), site.root.join("proj")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.create(
        &mut remote,
        &site_a.root.join("proj/file.txt"),
        b"v2 from a",
    );
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        fs::read(site.root.join("proj/file.txt")).unwrap(),
        b"v2 from a"
    );
}

/// A restart cannot read the rules file of `proj`, so what is below it is held. The user deletes the file. No rules
/// were known for the folder, so no rules changed, but the scan that lists the folder without a rules file ends the
/// hold, and a later remote edit arrives.
#[test]
fn deleting_a_rules_file_a_restart_cannot_read_ends_the_hold_of_its_folder() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("proj")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/file.txt"), b"v1");
    a.create(
        &mut remote,
        &site_a.root.join("proj/.koiignore"),
        b"*.log\n",
    );
    let (site, mut b) = linked(&mut remote, "b");
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    b.run(&mut remote);
    drop(b);
    fs::write(site.root.join("proj/.koiignore"), "*.log\n".repeat(200_000)).unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    fs::remove_file(site.root.join("proj/.koiignore")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.create(
        &mut remote,
        &site_a.root.join("proj/file.txt"),
        b"v2 from a",
    );
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        fs::read(site.root.join("proj/file.txt")).unwrap(),
        b"v2 from a"
    );
}

/// An effect creates the folder `b` from the server. The user then moves `a` into it and puts a rules file that cannot
/// be read in it. The batch that reports the move began before the effect ended, so its move is dropped and both paths
/// are read again. The removals the scan reported below `a` belong to the move and are dropped with it, so nothing
/// below the folder is deleted on the server.
#[test]
fn removals_below_a_folder_move_that_is_dropped_are_dropped_with_it() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("a")).unwrap();
    a.create(&mut remote, &site_a.root.join("a/x"), b"x");
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site_a.root.join("b")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.scan_until(|b| (b.observer.snapshot().entries).contains_key(Path::new("a/x")));
    let id = b.observer.snapshot().entries[Path::new("a")].id;
    let sections = b.folder.sections();
    let stamp = sections.stamp();
    b.pull(&remote);
    b.run(&mut remote);
    assert!(site.root.join("b").is_dir());
    fs::rename(site.root.join("a"), site.root.join("b/a")).unwrap();
    fs::write(site.root.join("b/a/.koiignore"), "*.log\n".repeat(200_000)).unwrap();
    let stale = Batch {
        stamp,
        answered: None,
        observation: crate::observe::Observation {
            changes: vec![
                Observed::Removed {
                    path: "a/x".into(),
                    kind: EntryKind::File,
                },
                Observed::Renamed {
                    from: "a".into(),
                    to: "b/a".into(),
                    kind: EntryKind::Dir,
                    id,
                    linked: false,
                },
            ],
            root: Ok(()),
            unreadable: vec![PathBuf::from("b/a")],
            scanned: b.observer.scanned(),
            busy: Vec::new(),
            rules: Vec::new(),
            read: Vec::new(),
            all_rules: false,
            mounts: BTreeSet::new(),
        },
    };
    let step = b.folder.observed(stale, 1);
    b.take(step).unwrap();
    assert!(!b.run(&mut remote).contains(&"send"));
    b.settle();
    b.run(&mut remote);
    let live = (b.folder.engine.trees.remote().iter())
        .any(|(_, node)| !node.trashed && node.shape.name == "x");
    assert!(live, "x stays on the server");
}

/// A folder the user deletes stays on the server while it holds a file the rules exclude here (design §5.10), so
/// nothing is left to send for it, and the folder is up to date.
#[test]
fn a_deleted_folder_that_keeps_an_excluded_file_on_the_server_leaves_the_folder_up_to_date() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("d")).unwrap();
    a.create(&mut remote, &site.root.join("d/a.log"), b"log");
    a.create(&mut remote, &site.root.join("d/b.txt"), b"text");
    a.create(&mut remote, &site.root.join(".koiignore"), b"*.log\n");
    fs::remove_dir_all(site.root.join("d")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    a.pull(&remote);
    a.run(&mut remote);
    a.settle();
    assert!(a.run(&mut remote).is_empty());
    let mut live: Vec<&str> = (a.folder.engine.trees.remote().iter())
        .filter(|(_, node)| !node.trashed)
        .map(|(_, node)| node.shape.name.as_str())
        .collect();
    live.sort();
    assert_eq!(live, [".koiignore", "a.log", "d"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// Device `a` deletes the folder `held` while `b` holds an entry L does not know there: a file `b`'s rules exclude,
/// or a symlink. `b` removes `held/a.txt` and keeps the folder with that entry, so the folder's removal fails and
/// is not planned again on a timer. The folder is up to date, and the status names the entry. Once the person removes
/// it, a restart removes the folder.
#[test]
fn a_remote_delete_of_a_folder_holding_an_entry_l_does_not_know_names_the_entry_until_it_is_removed()
 {
    for unknown in ["build.log", "link"] {
        let mut remote = Remote::new();
        let (site_a, mut a) = linked(&mut remote, "a");
        fs::create_dir(site_a.root.join("held")).unwrap();
        a.create(&mut remote, &site_a.root.join("held/a.txt"), b"synced");
        let sync = SyncRules {
            include: Vec::new(),
            exclude: vec!["*.log".into()],
        };
        let site = Site::new(&remote, "b");
        let mut b = site.start_with(&sync);
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        b.run(&mut remote);
        b.settle();
        let entry = site.root.join("held").join(unknown);
        match unknown {
            "link" => std::os::unix::fs::symlink("/etc/hosts", &entry).unwrap(),
            _ => fs::write(&entry, b"left out").unwrap(),
        }
        b.settle();
        assert_eq!(b.state(), FolderState::UpToDate, "{unknown}");

        fs::remove_dir_all(site_a.root.join("held")).unwrap();
        a.scan_until(|a| !a.work.is_empty());
        a.run(&mut remote);
        b.pull(&remote);
        let mut steps = 0;
        while let Some(work) = b.work.pop_front() {
            steps += 1;
            assert!(
                steps < 20,
                "{unknown}: the removal is planned again and again"
            );
            b.perform(work, &mut remote).unwrap();
        }
        b.settle();
        assert!(b.work.is_empty(), "{unknown}");
        assert!(fs::symlink_metadata(&entry).is_ok(), "{unknown} is kept");
        assert!(!site.root.join("held/a.txt").exists());
        let view = b.folder.view();
        assert_eq!(view.state, FolderState::UpToDate, "{unknown}");
        let problem = view.problem.unwrap_or_default();
        assert!(
            problem.contains(&format!("\"held\" (the server's change could not be made here: the folder holds entries this device does not sync: {unknown:?})")),
            "{problem}"
        );

        drop(b);
        fs::remove_file(&entry).unwrap();
        let mut b = site.start_with(&sync);
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        b.run(&mut remote);
        b.settle();
        assert!(!site.root.join("held").exists(), "{unknown}");
        let view = b.folder.view();
        assert_eq!(
            (view.state, view.problem),
            (FolderState::UpToDate, None),
            "{unknown}"
        );
    }
}

/// As above, with the excluded file one folder deeper. `held/sub` is the folder that cannot go, and `held` waits behind
/// it, so the folder is still up to date. Once the person removes the file, a restart removes both folders.
#[test]
fn a_remote_delete_of_a_folder_whose_subfolder_holds_an_entry_l_does_not_know_leaves_the_folder_up_to_date()
 {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site_a.root.join("held/sub")).unwrap();
    a.create(&mut remote, &site_a.root.join("held/a.txt"), b"synced");
    a.create(&mut remote, &site_a.root.join("held/sub/b.txt"), b"in sub");
    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["*.log".into()],
    };
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    assert!(site.root.join("held/sub/b.txt").exists());
    let entry = site.root.join("held/sub/build.log");
    fs::write(&entry, b"left out").unwrap();
    b.settle();

    fs::remove_dir_all(site_a.root.join("held")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    let mut steps = 0;
    while let Some(work) = b.work.pop_front() {
        steps += 1;
        assert!(steps < 20, "the removal is planned again and again");
        b.perform(work, &mut remote).unwrap();
    }
    b.settle();
    assert!(fs::read(&entry).is_ok());
    assert!(!site.root.join("held/sub/b.txt").exists());
    let view = b.folder.view();
    assert_eq!(view.state, FolderState::UpToDate);
    let problem = view.problem.unwrap_or_default();
    assert!(
        problem.contains(&format!(
            "{:?} (the server's change could not be made here: the folder holds entries this device does not sync: \
             \"build.log\")",
            Path::new("held").join("sub")
        )),
        "{problem}"
    );

    drop(b);
    fs::remove_file(&entry).unwrap();
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert!(!site.root.join("held").exists());
    let view = b.folder.view();
    assert_eq!((view.state, view.problem), (FolderState::UpToDate, None));
}

/// As above, with a second effect that fails for a moment: a download whose name an unobserved local file takes. Its
/// wait ends and unblocks it, while the stuck folder removal stays blocked.
#[test]
fn a_wait_for_another_failed_effect_leaves_a_folder_removal_that_entries_l_does_not_know_block_blocked()
 {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("held")).unwrap();
    a.create(&mut remote, &site_a.root.join("held/a.txt"), b"synced");
    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["*.log".into()],
    };
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    let held = node_named(&b, "held");
    fs::write(site.root.join("held/build.log"), b"left out").unwrap();
    b.settle();
    fs::remove_dir_all(site_a.root.join("held")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    assert!(b.folder.engine.effects.blocked().contains_key(&held));
    assert!(b.work.is_empty());

    a.create(&mut remote, &site_a.root.join("later.txt"), b"written on a");
    b.pull(&remote);
    fs::write(site.root.join("later.txt"), b"not observed yet").unwrap();
    let stage = b.work.pop_front().unwrap();
    assert!(matches!(stage, Work::Stage(_)));
    b.perform(stage, &mut remote).unwrap();
    let wait = b.work.pop_front().unwrap();
    assert!(matches!(wait, Work::RetryAfter(_)));
    b.perform(wait, &mut remote).unwrap();
    assert!(b.folder.engine.effects.blocked().contains_key(&held));
    assert_eq!(
        fs::read(site.root.join("held/build.log")).unwrap(),
        b"left out"
    );
}

/// The person deletes a folder whose removal is stuck on an entry L does not know. The scan takes the node out of
/// the blocked ones without planning anything for it, and the store holds it unblocked too, as every step checks.
#[test]
fn a_scan_that_unblocks_a_node_without_planning_an_effect_stores_it_unblocked() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("held")).unwrap();
    a.create(&mut remote, &site_a.root.join("held/a.txt"), b"synced");
    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["*.log".into()],
    };
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    let held = node_named(&b, "held");
    fs::write(site.root.join("held/build.log"), b"left out").unwrap();
    b.settle();
    fs::remove_dir_all(site_a.root.join("held")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    assert!(b.folder.engine.effects.blocked().contains_key(&held));

    fs::remove_dir_all(site.root.join("held")).unwrap();
    b.scan_until(|b| !b.folder.engine.effects.blocked().contains_key(&held));
    assert!(!site.stored().effects.blocked().contains_key(&held));
    b.settle();
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_file_whose_place_an_excluded_folder_holds_is_reported_while_its_folder_syncs() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"moved on a");
    let (site, mut b) = linked(&mut remote, "b");
    fs::rename(site_a.root.join("a"), site_a.root.join("x")).unwrap();
    a.settle();
    a.run(&mut remote);
    fs::create_dir(site_a.root.join("a")).unwrap();
    fs::rename(site_a.root.join("x"), site_a.root.join("a/ß")).unwrap();
    fs::write(site_a.root.join("a/a"), b"written on a").unwrap();
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), ["a", "a", "ß"]);

    let file = node_named(&b, "a");
    let parked = site.root.join(".koidrive/parked").join(&file);
    b.pull_at_once(&remote);
    while !site.root.join("a").is_dir() {
        let work = b.work.pop_front().expect("the folder is created");
        b.perform(work, &mut remote).unwrap();
    }
    assert_eq!(fs::read(&parked).unwrap(), b"moved on a");
    fs::create_dir(site.root.join("a/ß")).unwrap();
    b.scan_until(|b| {
        (b.folder.engine.trees.local().iter()).any(|(_, node)| node.shape.name == "ß")
    });
    drop(b);

    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["ss/".into()],
    };
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    fs::write(site.root.join("a/a"), b"edited on b").unwrap();
    b.settle();
    assert_eq!(b.run(&mut remote), ["upload", "send"]);
    b.settle();
    assert!(b.run(&mut remote).is_empty());
    a.pull_at_once(&remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(fs::read(site_a.root.join("a/a")).unwrap(), b"edited on b");

    let view = b.folder.view();
    assert_eq!(
        (view.state, view.problem),
        (
            FolderState::UpToDate,
            Some(format!(
                "1 item could not be synced: \"a/ß\" (its name here is taken by \"a/ß\", which this device's \
                 rules exclude; it is kept in \".koidrive/parked/{file}\"); run koi folder restart once it is fixed"
            ))
        )
    );
    assert_eq!(fs::read_dir(site.root.join("a/ß")).unwrap().count(), 0);
    assert_eq!(fs::read(&parked).unwrap(), b"moved on a");
    assert_eq!(remote.names(), ["a", "a", "ß"]);

    fs::remove_file(site.root.join("a/a")).unwrap();
    b.settle();
    assert_eq!(b.run(&mut remote), ["send"]);
    b.settle();
    assert!(b.run(&mut remote).is_empty());
    assert_eq!(remote.names(), ["a", "ß"]);
    a.pull_at_once(&remote);
    a.settle();
    a.run(&mut remote);
    assert!(!site_a.root.join("a/a").exists());
    assert_eq!(fs::read(site_a.root.join("a/ß")).unwrap(), b"moved on a");
    assert!(!site.root.join("a/a").exists());
}

/// Device `b` excludes the folder `q/x`. Device `a` renames that folder to `q/y`, then moves the file `p/f` to `q/x` and
/// deletes the folder `p`. On `b` the excluded folder keeps its place, so nothing can put the file at `q/x`, and `p`
/// can't be removed while the file is in it. The folder is up to date, and its problem names the file and the entry
/// that holds its name.
#[test]
fn a_folder_to_remove_that_holds_a_file_an_excluded_entry_keeps_out_lets_the_folder_settle() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    for dir in ["p", "q", "q/x"] {
        fs::create_dir(site_a.root.join(dir)).unwrap();
    }
    a.create(&mut remote, &site_a.root.join("p/f"), b"in p");
    let (site, b) = linked(&mut remote, "b");
    drop(b);
    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["x/".into()],
    };
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);

    fs::rename(site_a.root.join("q/x"), site_a.root.join("q/y")).unwrap();
    a.settle();
    a.run(&mut remote);
    fs::rename(site_a.root.join("p/f"), site_a.root.join("q/x")).unwrap();
    fs::remove_dir(site_a.root.join("p")).unwrap();
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), ["q", "x", "y"]);

    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    assert!(b.run(&mut remote).is_empty());

    let view = b.folder.view();
    assert_eq!(
        (view.state, view.problem),
        (
            FolderState::UpToDate,
            Some(
                "1 item could not be synced: \"p/f\" (its name here is taken by \"q/x\", which this device's rules \
                 exclude); run koi folder restart once it is fixed"
                    .into()
            )
        )
    );
    assert_eq!(fs::read(site.root.join("p/f")).unwrap(), b"in p");
    assert!(site.root.join("q/x").is_dir());
}

/// Device `b` excludes `skip/`. Device `a` moves the file `foo` to `skip/foo`, then makes a folder `foo` with the file
/// `foo/child` in it. On `b` the excluded file keeps the name `foo`, so the folder can't be created and the file in it
/// can't be written. The folder is up to date, and its problem names both and the entry that holds the name.
#[test]
fn a_file_in_a_folder_an_excluded_entry_keeps_out_is_named_with_it() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("foo"), b"kept on b");
    let (site, b) = linked(&mut remote, "b");
    drop(b);
    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["skip/".into()],
    };
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);

    fs::create_dir(site_a.root.join("skip")).unwrap();
    fs::rename(site_a.root.join("foo"), site_a.root.join("skip/foo")).unwrap();
    fs::create_dir(site_a.root.join("foo")).unwrap();
    fs::write(site_a.root.join("foo/child"), b"written on a").unwrap();
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), ["child", "foo", "foo", "skip"]);

    b.pull_at_once(&remote);
    b.run(&mut remote);
    b.settle();
    assert!(b.run(&mut remote).is_empty());

    let view = b.folder.view();
    assert_eq!(
        (view.state, view.problem),
        (
            FolderState::UpToDate,
            Some(
                "2 items could not be synced: \"foo\" (its name here is taken by \"foo\", which this device's rules \
                 exclude), \"foo/child\" (waits for \"foo\", whose name here is taken by an entry this device's rules \
                 exclude); run koi folder restart once they are fixed"
                    .into()
            )
        )
    );
    assert_eq!(fs::read(site.root.join("foo")).unwrap(), b"kept on b");
}

#[test]
fn a_batch_that_fails_the_root_check_stops_the_folder_before_l_loses_a_file() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");

    fs::remove_file(site.root.join(koidrive_fs::PRIVATE_DIR).join("space.json")).unwrap();
    fs::remove_file(site.root.join("notes.txt")).unwrap();
    let mut stop = None;
    eventually("the root check stops the folder", || {
        stop = a.try_scan(Duration::from_millis(100)).err();
        stop.is_some()
    });
    let stop = stop.unwrap();
    assert!(
        matches!(stop, Stop::Root(RootProblem::NoMarker)),
        "{stop:?}"
    );
    assert!(local_holds(&site, "notes.txt"));
    assert!(a.work.is_empty());
    assert_eq!(remote.names(), ["notes.txt"]);
}

#[test]
fn a_folder_the_scan_may_not_read_keeps_its_files_in_the_space() {
    use std::os::unix::fs::PermissionsExt;

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let folder = site.root.join("private");
    fs::create_dir(&folder).unwrap();
    a.create(&mut remote, &folder.join("secret.txt"), b"written on a");
    let sent = a.sent.len();

    fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&folder).is_ok() {
        assert!(rustix::process::geteuid().is_root());
        eprintln!("SKIPPED: running as root, so permissions do not block reads");
        return;
    }
    eventually("the scan reports the folder it may not read", || {
        a.try_scan(Duration::from_millis(100))
            .unwrap()
            .contains(&"private".into())
    });
    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(a.sent.len(), sent);
    assert!(local_holds(&site, "secret.txt"));

    fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(a.sent.len(), sent);
    assert_eq!(remote.names(), ["private", "secret.txt"]);
}

#[test]
fn a_restart_before_the_upload_of_a_commit_sends_it_once_with_its_chunks() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert!(matches!(a.work.front(), Some(Work::Upload(_))));
    let built = a
        .folder
        .engine
        .commits
        .in_flight()
        .unwrap()
        .commit
        .commit_id
        .clone();
    drop(a);

    let mut a = site.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    a.settle();
    assert!(a.work.is_empty());
    assert!(a.sent.iter().all(|sent| sent.commit_id == built));
    assert_eq!(remote.space.head_seq(), 1);
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_rules_file_named_in_another_case_is_downloaded_and_in_force_first() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("cache.tmp"), b"cache");
    a.create(&mut remote, &site_a.root.join(".KOIIGNORE"), b"*.tmp\n");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage"]);
    assert_eq!(names(&site.root), [".KOIIGNORE"]);
    assert!(b.folder.disk.awaits_rules());

    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());
    b.settle();
    assert!(b.work.is_empty() && b.sent.is_empty());
    assert_eq!(names(&site.root), [".KOIIGNORE"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn planning_goes_on_after_a_folder_move_once_a_scan_that_found_nothing_is_reported() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("old")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    assert_eq!(names(&site.root), ["old"]);

    fs::rename(site_a.root.join("old"), site_a.root.join("new")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    b.pull(&remote);
    assert_eq!(b.run(&mut remote), ["stage"]);
    assert_eq!(names(&site.root), ["new"]);
    assert!(
        b.folder.disk.awaits_rules(),
        "the trees hold a folder's rules by its path"
    );
    assert_eq!(b.state(), FolderState::CatchingUp);

    a.create(&mut remote, &site_a.root.join("later.txt"), b"later");
    b.pull(&remote);
    assert!(b.work.is_empty(), "nothing is planned before the scan");

    let sections = b.folder.sections();
    let batch = (sections
        .observe(&mut b.observer, Duration::from_secs(10))
        .unwrap())
    .expect("a reset is answered");
    assert!(batch.observation.changes.is_empty() && batch.observation.rules.is_empty());
    let step = b.folder.observed(batch, 1);
    b.take(step).unwrap();
    assert_eq!(b.run(&mut remote), ["stage"]);
    assert_eq!(names(&site.root), ["later.txt", "new"]);
}

#[test]
fn a_remote_edit_of_an_excluded_file_is_not_applied_while_its_folder_moves() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("proj")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/build.log"), b"v1");
    let (site, mut b) = linked(&mut remote, "b");
    let (site_c, mut c) = linked(&mut remote, "c");
    b.settle();
    c.settle();

    a.create(
        &mut remote,
        &site_a.root.join("proj/.koiignore"),
        b"*.log\n",
    );
    b.pull(&remote);
    assert_eq!(b.run(&mut remote), ["stage"]);
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());

    fs::rename(site_a.root.join("proj"), site_a.root.join("work")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    c.create(
        &mut remote,
        &site_c.root.join("proj/build.log"),
        b"v2 from c",
    );

    b.pull(&remote);
    assert_eq!(
        b.run(&mut remote),
        ["stage"],
        "only the folder moves until its rules file is read at the new path"
    );
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());
    b.settle();
    assert!(b.work.is_empty() && b.sent.is_empty());
    assert_eq!(fs::read(site.root.join("work/build.log")).unwrap(), b"v1");
}

#[test]
fn a_batch_scanned_before_a_rules_file_was_written_does_not_end_the_wait_for_its_rules() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("cache.tmp"), b"cache");
    a.create(&mut remote, &site_a.root.join(".koiignore"), b"*.tmp\n");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    fs::write(site.root.join("mine.txt"), b"mine").unwrap();
    let sections = b.folder.sections();
    let mut batch = None;
    eventually("the observer reports a batch", || {
        batch = sections
            .observe(&mut b.observer, Duration::from_millis(100))
            .unwrap();
        batch.is_some()
    });
    let batch = batch.unwrap();
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage"]);
    assert!(b.folder.disk.awaits_rules());

    let step = b.folder.observed(batch, 1);
    b.take(step).unwrap();
    assert!(
        b.folder.disk.awaits_rules(),
        "the batch was scanned before the rules file was there"
    );
    assert!(b.work.is_empty());

    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert_eq!(b.run(&mut remote), ["upload", "send"]);
    assert_eq!(names(&site.root), [".koiignore", "mine.txt"]);
}

/// The observer's thread takes its resets, and the effect that moves the folder ends before the thread reads its
/// stamp. The batch that follows began after the effect, but its scans were never asked to read the moved folder.
#[test]
fn a_batch_that_was_not_asked_to_scan_a_moved_folder_does_not_end_the_wait_for_its_rules() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("proj")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/build.log"), b"v1");
    let (site, mut b) = linked(&mut remote, "b");
    let (site_c, mut c) = linked(&mut remote, "c");
    b.settle();
    c.settle();
    a.create(
        &mut remote,
        &site_a.root.join("proj/.koiignore"),
        b"*.log\n",
    );
    b.pull(&remote);
    assert_eq!(b.run(&mut remote), ["stage"]);
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());
    fs::rename(site_a.root.join("proj"), site_a.root.join("work")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    c.create(
        &mut remote,
        &site_c.root.join("proj/build.log"),
        b"v2 from c",
    );

    let sections = b.folder.sections();
    let (resets, requests) = sections.take_resets();
    assert!(resets.is_empty() && b.observer.answered());
    b.pull(&remote);
    assert_eq!(b.run(&mut remote), ["stage"]);
    assert!(b.folder.disk.awaits_rules());
    let unrelated = Batch {
        stamp: sections.stamp(),
        answered: Some(requests),
        observation: crate::observe::Observation {
            changes: Vec::new(),
            root: Ok(()),
            unreadable: vec![PathBuf::from("unrelated")],
            scanned: b.observer.scanned(),
            busy: Vec::new(),
            rules: Vec::new(),
            read: Vec::new(),
            all_rules: false,
            mounts: BTreeSet::new(),
        },
    };
    let step = b.folder.observed(unrelated, 1);
    b.take(step).unwrap();
    assert!(
        b.folder.disk.awaits_rules(),
        "no scan has read the rules file at the new path"
    );
    assert!(b.run(&mut remote).is_empty());
    assert_eq!(fs::read(site.root.join("work/build.log")).unwrap(), b"v1");

    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());
    assert_eq!(fs::read(site.root.join("work/build.log")).unwrap(), b"v1");
}
