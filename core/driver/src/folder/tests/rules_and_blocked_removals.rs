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
        ..SyncRules::default()
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
    assert!(b.folder.disk.awaits_scan());
    assert_eq!(b.state(), FolderState::CatchingUp);

    b.scan_until(|b| !b.folder.disk.awaits_scan());
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
    b.scan_until(|b| !b.folder.disk.awaits_scan());
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
    b.scan_until(|b| !b.folder.disk.awaits_scan());
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
    b.scan_until(|b| !b.folder.disk.awaits_scan());
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
    b.scan_until(|b| !b.folder.disk.awaits_scan());
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
        unread: (0, 0),
        answered: Default::default(),
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
            pending: Vec::new(),
            waiting: Default::default(),
            rules: Vec::new(),
            read: Vec::new(),
            all_rules: false,
            mounts: BTreeMap::new(),
            detached: BTreeMap::new(),
            ctimes: BTreeMap::new(),
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

#[test]
fn a_removal_dropped_while_excluded_stays_unobserved_until_the_reset_scan_answers() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site.root.join("d")).unwrap();
    b.create(&mut remote, &site.root.join("d/x.log"), b"x");
    b.create(&mut remote, &site.root.join(".koiignore"), b"*.log\n");
    b.rest(&mut remote);
    fs::create_dir(site_a.root.join("n")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    let sections = b.folder.sections();
    let stamp = sections.stamp();
    let id = b.observer.snapshot().entries[Path::new("d")].id;
    b.pull(&remote);
    b.run(&mut remote);
    assert!(site.root.join("n").is_dir());
    fs::rename(site.root.join("d"), site.root.join("n/d")).unwrap();
    fs::remove_file(site.root.join("n/d/x.log")).unwrap();
    fs::write(site.root.join(".koiignore"), b"other.txt\n").unwrap();
    let batch = |stamp, changes, rules, read| Batch {
        stamp,
        unread: (0, 0),
        answered: Default::default(),
        observation: crate::observe::Observation {
            changes,
            root: Ok(()),
            unreadable: Vec::new(),
            scanned: b.observer.scanned(),
            busy: Vec::new(),
            pending: Vec::new(),
            waiting: Default::default(),
            rules,
            read,
            all_rules: false,
            mounts: BTreeMap::new(),
            detached: BTreeMap::new(),
            ctimes: BTreeMap::new(),
        },
    };
    let stale = batch(
        stamp,
        vec![
            Observed::Renamed {
                from: "d".into(),
                to: "n/d".into(),
                kind: EntryKind::Dir,
                id,
                linked: false,
            },
            Observed::Removed {
                path: "n/d/x.log".into(),
                kind: EntryKind::File,
            },
        ],
        vec![crate::observe::RulesChange {
            dir: PathBuf::new(),
            source: koidrive_fs::Source::Koiignore,
            rules: Some(koidrive_engine::ignore::RuleFile::parse(b"other.txt\n")),
        }],
        Vec::new(),
    );
    let before_resets = batch(sections.stamp(), Vec::new(), Vec::new(), vec!["d".into()]);
    let step = b.folder.observed(stale, 2);
    b.take(step).unwrap();
    let step = b.folder.observed(before_resets, 2);
    b.take(step).unwrap();
    b.settle();
    b.rest(&mut remote);
    assert!(!b.sent.iter().flat_map(kinds).any(|kind| kind == "delete"));
    assert_eq!(fs::read(site.root.join("n/d/x.log")).unwrap(), b"x");
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

#[test]
fn a_remote_delete_of_a_folder_holding_an_entry_l_does_not_know_keeps_the_folder_with_it()
 {
    for unknown in ["build.log", "link"] {
        let mut remote = Remote::new();
        let (site_a, mut a) = linked(&mut remote, "a");
        fs::create_dir(site_a.root.join("held")).unwrap();
        a.create(&mut remote, &site_a.root.join("held/a.txt"), b"synced");
        let sync = SyncRules {
            include: Vec::new(),
            exclude: vec!["*.log".into()],
            ..SyncRules::default()
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
        assert_eq!(
            (view.state, view.problem),
            (FolderState::UpToDate, None),
            "{unknown}"
        );
        assert!(b.sent.is_empty(), "{unknown}");

        drop(b);
        fs::remove_file(&entry).unwrap();
        let mut b = site.start_with(&sync);
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        b.run(&mut remote);
        b.settle();
        assert!(site.root.join("held").is_dir(), "{unknown}");
        assert!(b.sent.is_empty(), "{unknown}");
        assert!(remote.names().is_empty(), "{unknown}");
        let view = b.folder.view();
        assert_eq!(
            (view.state, view.problem),
            (FolderState::UpToDate, None),
            "{unknown}"
        );
    }
}

#[test]
fn a_remote_delete_of_a_folder_whose_subfolder_holds_an_entry_l_does_not_know_keeps_both_folders()
 {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site_a.root.join("held/sub")).unwrap();
    a.create(&mut remote, &site_a.root.join("held/a.txt"), b"synced");
    a.create(&mut remote, &site_a.root.join("held/sub/b.txt"), b"in sub");
    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["*.log".into()],
        ..SyncRules::default()
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
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    b.settle();
    assert!(fs::read(&entry).is_ok());
    assert!(!site.root.join("held/sub/b.txt").exists());
    let view = b.folder.view();
    assert_eq!((view.state, view.problem), (FolderState::UpToDate, None));
    assert!(b.sent.is_empty());

    drop(b);
    fs::remove_file(&entry).unwrap();
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert!(site.root.join("held/sub").is_dir());
    assert!(b.sent.is_empty());
    assert!(remote.names().is_empty());
    let view = b.folder.view();
    assert_eq!((view.state, view.problem), (FolderState::UpToDate, None));
}

#[test]
fn a_wait_for_another_failed_effect_plans_nothing_for_a_kept_folder()
 {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("held")).unwrap();
    a.create(&mut remote, &site_a.root.join("held/a.txt"), b"synced");
    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["*.log".into()],
        ..SyncRules::default()
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
    assert!(!b.folder.engine.effects.blocked().contains_key(&held));
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
    assert!(!b.folder.engine.effects.blocked().contains_key(&held));
    assert!(
        (b.folder.engine.effects.in_flight().iter()).all(|record| record.node != held),
        "the kept folder's removal is not planned again"
    );
    assert_eq!(
        fs::read(site.root.join("held/build.log")).unwrap(),
        b"left out"
    );
}

#[test]
fn a_scan_that_unblocks_a_node_without_planning_an_effect_stores_it_unblocked() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("x"), b"synced");
    let (site, mut b) = linked(&mut remote, "b");
    let x = node_named(&b, "x");
    fs::rename(site_a.root.join("x"), site_a.root.join("y")).unwrap();
    a.settle();
    a.run(&mut remote);
    b.pull(&remote);
    fs::write(site.root.join("y"), b"not observed yet").unwrap();
    let stage = b.work.pop_front().unwrap();
    assert!(matches!(stage, Work::Stage(_)));
    b.perform(stage, &mut remote).unwrap();
    assert!(b.folder.engine.effects.blocked().contains_key(&x));

    fs::remove_file(site.root.join("x")).unwrap();
    b.scan_until(|b| !b.folder.engine.effects.blocked().contains_key(&x));
    assert!(!site.stored().effects.blocked().contains_key(&x));
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
    let parked = site.root.join(".koidrive/parked").join(file.to_string());
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
        ..SyncRules::default()
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
    assert_eq!(b.run(&mut remote), ["retry", "send"]);
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
        ..SyncRules::default()
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
        ..SyncRules::default()
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
    assert!(b.folder.disk.awaits_scan());

    b.scan_until(|b| !b.folder.disk.awaits_scan());
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
        b.folder.disk.awaits_scan(),
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
    b.scan_until(|b| !b.folder.disk.awaits_scan());
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
    b.scan_until(|b| !b.folder.disk.awaits_scan());
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
    assert!(b.folder.disk.awaits_scan());

    let step = b.folder.observed(batch, 1);
    b.take(step).unwrap();
    assert!(
        b.folder.disk.awaits_scan(),
        "the batch was scanned before the rules file was there"
    );
    assert!(b.work.is_empty());

    b.scan_until(|b| !b.folder.disk.awaits_scan());
    assert_eq!(b.run(&mut remote), ["upload", "send"]);
    assert_eq!(names(&site.root), [".koiignore", "mine.txt"]);
}

#[test]
fn a_downloaded_rules_file_ends_the_wait_for_its_scan_while_another_folder_stays_busy() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("logs")).unwrap();
    a.create(&mut remote, &site_a.root.join("logs/app.log"), b"started\n");
    a.create(&mut remote, &site_a.root.join("cache.tmp"), b"cache");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    b.observer.keep_pending(PathBuf::from("logs"));
    a.create(&mut remote, &site_a.root.join("logs/a.log"), b"from a");
    b.pull(&remote);
    b.run(&mut remote);
    assert_eq!(fs::read(site.root.join("logs/a.log")).unwrap(), b"from a");

    a.create(&mut remote, &site_a.root.join(".koiignore"), b"*.tmp\n");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"notes");
    b.pull(&remote);
    assert_eq!(b.run(&mut remote), ["stage"]);
    assert!(b.folder.disk.awaits_scan());

    b.scan_until(|b| !b.folder.disk.awaits_scan());
    b.run(&mut remote);
    assert_eq!(fs::read(site.root.join("notes.txt")).unwrap(), b"notes");
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
    b.scan_until(|b| !b.folder.disk.awaits_scan());
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
    assert!(b.folder.disk.awaits_scan());
    let unrelated = Batch {
        stamp: sections.stamp(),
        unread: (0, 0),
        answered: crate::disk::Answers {
            through: requests,
            open: Vec::new(),
        },
        observation: crate::observe::Observation {
            changes: Vec::new(),
            root: Ok(()),
            unreadable: vec![PathBuf::from("unrelated")],
            scanned: b.observer.scanned(),
            busy: Vec::new(),
            pending: Vec::new(),
            waiting: Default::default(),
            rules: Vec::new(),
            read: Vec::new(),
            all_rules: false,
            mounts: BTreeMap::new(),
            detached: BTreeMap::new(),
            ctimes: BTreeMap::new(),
        },
    };
    let step = b.folder.observed(unrelated, 1);
    b.take(step).unwrap();
    assert!(
        b.folder.disk.awaits_scan(),
        "no scan has read the rules file at the new path"
    );
    assert!(b.run(&mut remote).is_empty());
    assert_eq!(fs::read(site.root.join("work/build.log")).unwrap(), b"v1");

    b.scan_until(|b| !b.folder.disk.awaits_scan());
    assert!(b.run(&mut remote).is_empty());
    assert_eq!(fs::read(site.root.join("work/build.log")).unwrap(), b"v1");
}

#[cfg(target_os = "linux")]
#[test]
fn a_remote_delete_of_a_folder_holding_a_symlink_with_a_name_that_is_not_utf8_keeps_the_folder() {
    use std::os::unix::ffi::OsStrExt;
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("held")).unwrap();
    a.create(&mut remote, &site_a.root.join("held/a.txt"), b"synced");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    let entry = site.root.join("held").join(std::ffi::OsStr::from_bytes(b"link\xff"));
    std::os::unix::fs::symlink("/etc/hosts", &entry).unwrap();
    b.settle();

    fs::remove_dir_all(site_a.root.join("held")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    assert!(fs::symlink_metadata(&entry).is_ok());
    assert!(!site.root.join("held/a.txt").exists());
    let view = b.folder.view();
    assert_eq!((view.state, view.problem), (FolderState::UpToDate, None));
    assert!(b.sent.is_empty(), "{:?}", b.sent);
}

fn excluded_dist(count: usize) -> (Remote, Site, Vec<(String, String)>) {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir_all(site.root.join("dist/sub")).unwrap();
    for n in 0..count {
        let dir = if n % 2 == 0 { "dist" } else { "dist/sub" };
        fs::write(
            site.root.join(format!("{dir}/f{n}.docx")),
            format!("report {n}"),
        )
        .unwrap();
    }
    b.rescan();
    b.rest(&mut remote);
    let synced = files(&site.root);
    assert_eq!(synced.len(), count);
    drop(b);
    (remote, site, synced)
}

fn started(site: &Site, remote: &mut Remote, sync: &SyncRules) -> Device {
    let mut device = site.start_with(sync);
    device.connect(remote);
    device.scan_until(|device| device.folder.scanned);
    device.rest(remote);
    device
}

fn dist_left_out() -> SyncRules {
    SyncRules {
        exclude: vec!["dist/".into()],
        ..SyncRules::default()
    }
}

fn a_node_deleted_while_excluded_downloads_again_when_the_rules_let_it_in(count: usize) {
    let (mut remote, site, synced) = excluded_dist(count);
    let server = remote.tree();

    let mut b = started(&site, &mut remote, &dist_left_out());
    fs::remove_dir_all(site.root.join("dist")).unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert!(b.sent.is_empty(), "nothing is sent while dist is left out");
    drop(b);

    let b = started(&site, &mut remote, &SyncRules::default());
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert!(!sent.contains(&"delete"), "sent {sent:?}");
    assert_eq!(remote.tree(), server);
    assert_eq!(files(&site.root), synced);
    assert!(trashed(&site.root).is_empty());
    assert!(shown(&site).is_empty(), "no question opens");
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_folder_of_2000_files_deleted_while_excluded_downloads_again_when_the_rules_let_it_in() {
    a_node_deleted_while_excluded_downloads_again_when_the_rules_let_it_in(2000);
}

#[test]
fn a_node_deleted_after_the_rules_let_it_in_again_is_deleted_and_an_edit_made_while_excluded_is_uploaded()
 {
    let (mut remote, site, _) = excluded_dist(3);
    let mut b = started(&site, &mut remote, &dist_left_out());
    fs::write(site.root.join("dist/f0.docx"), "edited while left out").unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert!(b.sent.is_empty());
    drop(b);

    let mut b = started(&site, &mut remote, &SyncRules::default());
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert_eq!(sent, ["update content"]);
    assert_eq!(
        remote.names(),
        ["dist", "f0.docx", "f1.docx", "f2.docx", "sub"]
    );

    fs::remove_dir_all(site.root.join("dist")).unwrap();
    b.rescan();
    b.rest(&mut remote);
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert_eq!(sent, ["update content", "delete"]);
    assert!(remote.names().is_empty());
    assert!(files(&site.root).is_empty());
}

fn changed_while_left_out(
    change: impl FnOnce(&Site),
    stopped: bool,
) -> (Device, Site, Vec<(String, String)>) {
    let (mut remote, site, synced) = excluded_dist(3);
    let server = remote.tree();
    let mut b = started(&site, &mut remote, &dist_left_out());
    if stopped {
        drop(b);
        change(&site);
    } else {
        change(&site);
        b.rescan();
        b.rest(&mut remote);
        assert!(b.sent.is_empty(), "nothing is sent while dist is left out");
        drop(b);
    }
    let mut b = started(&site, &mut remote, &SyncRules::default());
    b.rest(&mut remote);
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert!(!sent.contains(&"delete"), "sent {sent:?}");
    assert!(shown(&site).is_empty(), "no question opens");
    assert_eq!(
        remote.tree(),
        server,
        "every node keeps its place on the server"
    );
    assert_eq!(b.state(), FolderState::UpToDate);
    (b, site, synced)
}

fn rebuild_dist(site: &Site, text: impl Fn(usize) -> String) {
    fs::remove_dir_all(site.root.join("dist")).unwrap();
    fs::create_dir_all(site.root.join("dist/sub")).unwrap();
    for n in 0..3 {
        let dir = if n % 2 == 0 { "dist" } else { "dist/sub" };
        fs::write(site.root.join(format!("{dir}/f{n}.docx")), text(n)).unwrap();
    }
}

#[test]
fn a_file_removed_inside_an_excluded_folder_downloads_again_when_the_rules_let_it_in() {
    let (b, site, synced) = changed_while_left_out(
        |site| fs::remove_file(site.root.join("dist/f2.docx")).unwrap(),
        false,
    );
    assert!(b.sent.is_empty(), "sent {:?}", b.sent);
    assert_eq!(files(&site.root), synced);
    assert!(trashed(&site.root).is_empty());
}

#[test]
fn an_excluded_folder_removed_while_koidrive_is_stopped_downloads_again_when_the_rules_let_it_in() {
    let (b, site, synced) = changed_while_left_out(
        |site| fs::remove_dir_all(site.root.join("dist")).unwrap(),
        true,
    );
    assert!(b.sent.is_empty(), "sent {:?}", b.sent);
    assert_eq!(files(&site.root), synced);
}

#[test]
fn an_excluded_folder_cleaned_and_partly_rebuilt_uploads_the_rebuilt_file_and_downloads_the_rest() {
    let (b, site, synced) = changed_while_left_out(
        |site| {
            fs::remove_dir_all(site.root.join("dist")).unwrap();
            fs::create_dir_all(site.root.join("dist/sub")).unwrap();
            fs::write(site.root.join("dist/f0.docx"), "rebuilt").unwrap();
        },
        false,
    );
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert_eq!(sent, ["update content"]);
    let wanted: Vec<(String, String)> = (synced.into_iter())
        .map(|(path, text)| match path.as_str() {
            "dist/f0.docx" => (path, "rebuilt".into()),
            _ => (path, text),
        })
        .collect();
    assert_eq!(files(&site.root), wanted);
}

#[test]
fn a_build_folder_rebuilt_while_excluded_replaces_the_server_copy_when_the_rules_let_it_in() {
    let (b, site, _) =
        changed_while_left_out(|site| rebuild_dist(site, |n| format!("rebuilt {n}")), false);
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert_eq!(sent, ["update content"; 3]);
    let local = files(&site.root);
    assert_eq!(local.len(), 3);
    assert!(
        (local.iter())
            .all(|(path, text)| !path.contains("conflict") && text.starts_with("rebuilt")),
        "{local:?}"
    );
}

#[test]
fn a_build_folder_rebuilt_with_the_same_files_while_excluded_sends_nothing_when_the_rules_let_it_in()
 {
    let (b, site, synced) =
        changed_while_left_out(|site| rebuild_dist(site, |n| format!("report {n}")), false);
    assert!(b.sent.is_empty(), "sent {:?}", b.sent);
    assert_eq!(files(&site.root), synced);
}

fn rebuilt_under_another_spelling(server_name: &str, local_name: &str, synced_name: &str) {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::write(site.root.join("dist").join(server_name), "report").unwrap();
    b.rescan();
    b.rest(&mut remote);
    drop(b);
    let server = remote.tree();
    let id = server[&format!("dist/{server_name}")].clone();
    let mut b = started(&site, &mut remote, &dist_left_out());
    fs::remove_dir_all(site.root.join("dist")).unwrap();
    b.rescan();
    b.rest(&mut remote);
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::write(site.root.join("dist").join(local_name), "rebuilt").unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert!(b.sent.is_empty(), "nothing is sent while dist is left out");
    drop(b);

    let mut b = started(&site, &mut remote, &SyncRules::default());
    b.rest(&mut remote);
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert!(!sent.contains(&"delete"), "sent {sent:?}");
    assert!(shown(&site).is_empty(), "no question opens");
    assert_eq!(
        files(&site.root),
        [(format!("dist/{local_name}"), "rebuilt".to_owned())]
    );
    assert_eq!(
        remote.tree().into_values().collect::<Vec<_>>(),
        server.into_values().collect::<Vec<_>>(),
        "the rebuilt file is the server's node"
    );
    assert_eq!(
        remote.tree().into_keys().collect::<Vec<_>>(),
        ["dist".to_owned(), format!("dist/{synced_name}")]
    );
    assert_eq!(server_size(&remote, &id), "rebuilt".len() as u64);
}

#[test]
fn a_file_rebuilt_while_excluded_with_its_name_in_another_case_replaces_the_server_copy() {
    rebuilt_under_another_spelling("f0.docx", "F0.docx", "F0.docx");
}

#[test]
fn a_file_rebuilt_while_excluded_with_its_name_in_another_normalization_replaces_the_server_copy() {
    rebuilt_under_another_spelling("caf\u{e9}.docx", "cafe\u{301}.docx", "caf\u{e9}.docx");
}

fn relinked(site: &Site, remote: &Remote) -> Site {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.sqlite");
    let space_id = remote.space.space_id().to_owned();
    let found_marker = koidrive_fs::read_marker(&site.root)
        .unwrap()
        .is_some_and(|marker| marker.space_id == space_id);
    if !found_marker {
        let marker = Marker {
            space_id: space_id.clone(),
            effects: 0,
            token: None,
        };
        write_marker(&site.root, &marker).unwrap();
    }
    let link = Link {
        space_id,
        found_marker,
        ..site.link.clone()
    };
    Store::open(&db)
        .unwrap()
        .transaction(|tx| tx.set_link(&link))
        .unwrap();
    Site {
        _dir: dir,
        root: site.root.clone(),
        db,
        link,
        caller: site.caller.clone(),
    }
}

#[test]
fn a_folder_linked_again_keeps_no_mark_from_its_old_link() {
    for same_space in [true, false] {
        let (mut remote, site, synced) = excluded_dist(3);
        let mut b = started(&site, &mut remote, &dist_left_out());
        fs::remove_dir_all(site.root.join("dist")).unwrap();
        b.rescan();
        b.rest(&mut remote);
        assert!(!site.stored().trees.state().unobserved.is_empty());
        drop(b);

        let mut other = Remote::new_at(NOW + 1);
        assert_ne!(other.space.space_id(), remote.space.space_id());
        let target = if same_space { &mut remote } else { &mut other };
        let again = relinked(&site, target);
        let c = started(&again, target, &SyncRules::default());
        let stored = again.stored();
        assert!(
            stored.trees.state().unobserved.is_empty(),
            "same_space={same_space}"
        );
        assert!(c.sent.is_empty(), "same_space={same_space}: {:?}", c.sent);
        assert!(shown(&again).is_empty(), "same_space={same_space}");
        assert!(
            (stored.trees.local().iter()).all(|(id, _)| !stored.trees.held(id)),
            "same_space={same_space}"
        );
        match same_space {
            true => assert_eq!(files(&site.root), synced),
            false => {
                assert!(files(&site.root).is_empty());
                assert!(target.names().is_empty());
            }
        }
    }
}

#[test]
fn a_rescan_that_finds_nothing_new_writes_nothing_to_the_store_while_nodes_are_unobserved() {
    let (mut remote, site, _) = excluded_dist(3);
    let mut b = started(&site, &mut remote, &dist_left_out());
    assert_eq!(site.stored().trees.state().unobserved.len(), 5);
    let watcher = rusqlite::Connection::open(&site.db).unwrap();
    let version =
        || -> i64 { (watcher.query_row("PRAGMA data_version", [], |row| row.get(0))).unwrap() };
    let before = version();
    b.rescan();
    b.rest(&mut remote);
    assert!(b.sent.is_empty());
    assert_eq!(version(), before, "the rescan wrote to the store");
}

#[test]
fn a_synced_file_moved_into_an_ignored_folder_and_deleted_there_comes_back() {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::write(site.root.join("report.docx"), "report").unwrap();
    b.rescan();
    b.rest(&mut remote);
    drop(b);
    let mut b = started(&site, &mut remote, &dist_left_out());
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::rename(
        site.root.join("report.docx"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    b.rescan();
    b.rest(&mut remote);
    fs::remove_file(site.root.join("dist/report.docx")).unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert!(b.sent.is_empty());
    drop(b);

    for sync in [dist_left_out(), SyncRules::default()] {
        let b = started(&site, &mut remote, &sync);
        let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
        assert!(!sent.contains(&"delete"), "{:?}: {sent:?}", sync.exclude);
        assert!(remote.names().contains(&"report.docx".to_owned()));
        assert_eq!(
            files(&site.root),
            [("report.docx".to_owned(), "report".to_owned())],
            "{:?}",
            sync.exclude
        );
    }
}

#[test]
fn a_synced_file_moved_into_an_ignored_folder_and_deleted_there_while_running_comes_back() {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::write(site.root.join("report.docx"), "report").unwrap();
    b.settle();
    b.rest(&mut remote);
    drop(b);
    let mut b = started(&site, &mut remote, &dist_left_out());
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::rename(
        site.root.join("report.docx"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    b.settle();
    b.rest(&mut remote);
    fs::remove_file(site.root.join("dist/report.docx")).unwrap();
    for _ in 0..10 {
        b.settle();
        b.rest(&mut remote);
    }
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert!(!sent.contains(&"delete"), "{sent:?}");
    assert!(remote.names().contains(&"report.docx".to_owned()));
    assert_eq!(
        files(&site.root),
        [("report.docx".to_owned(), "report".to_owned())],
        "{sent:?}"
    );
}

#[test]
fn a_synced_file_found_in_an_ignored_folder_at_start_and_deleted_there_while_running_comes_back() {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::write(site.root.join("report.docx"), "report").unwrap();
    b.settle();
    b.rest(&mut remote);
    drop(b);
    let mut b = started(&site, &mut remote, &dist_left_out());
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::rename(
        site.root.join("report.docx"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    b.settle();
    b.rest(&mut remote);
    drop(b);
    let mut b = started(&site, &mut remote, &dist_left_out());
    b.rest(&mut remote);
    assert_eq!(
        files(&site.root),
        [("dist/report.docx".to_owned(), "report".to_owned())]
    );
    fs::remove_file(site.root.join("dist/report.docx")).unwrap();
    for _ in 0..10 {
        b.settle();
        b.rest(&mut remote);
    }
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert!(!sent.contains(&"delete"), "{sent:?}");
    assert_eq!(
        files(&site.root),
        [("report.docx".to_owned(), "report".to_owned())],
        "{sent:?}"
    );
}

fn moved_into_a_left_out_folder(remote: &mut Remote, to: &str) -> (Site, Device) {
    let (site, mut b) = linked(remote, "b");
    fs::write(site.root.join("report.docx"), "report").unwrap();
    b.settle();
    b.rest(remote);
    drop(b);
    let mut b = started(&site, remote, &dist_left_out());
    fs::create_dir_all(site.root.join(to).parent().unwrap()).unwrap();
    fs::rename(site.root.join("report.docx"), site.root.join(to)).unwrap();
    b.settle();
    b.rest(remote);
    (site, b)
}

#[test]
fn a_copy_in_a_left_out_folder_saved_by_rename_while_stopped_stays_the_only_copy() {
    use std::os::unix::fs::MetadataExt;
    let mut remote = Remote::new();
    let (site, b) = moved_into_a_left_out_folder(&mut remote, "dist/report.docx");
    drop(b);
    fs::write(site.root.join("dist/.~report.tmp"), "edited").unwrap();
    fs::rename(
        site.root.join("dist/.~report.tmp"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    let mut b = started(&site, &mut remote, &dist_left_out());
    for _ in 0..3 {
        b.settle();
        b.rest(&mut remote);
    }
    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert_eq!(
        files(&site.root),
        [("dist/report.docx".to_owned(), "edited".to_owned())]
    );
    let ino = fs::metadata(site.root.join("dist/report.docx"))
        .unwrap()
        .ino();
    let recorded = b.folder.engine.recorded();
    assert_eq!(recorded[Path::new("report.docx")].id.ino, u128::from(ino));
}

fn settled_after_a_start(site: &Site, remote: &mut Remote, sync: &SyncRules) {
    let mut b = started(site, remote, sync);
    for _ in 0..3 {
        b.settle();
        b.rest(remote);
    }
}

#[test]
fn a_copy_renamed_aside_in_a_left_out_folder_while_stopped_keeps_its_node() {
    let mut remote = Remote::new();
    let (site, b) = moved_into_a_left_out_folder(&mut remote, "dist/report.docx");
    let id = remote.tree()["report.docx"].clone();
    drop(b);
    fs::rename(
        site.root.join("dist/report.docx"),
        site.root.join("dist/kept.docx"),
    )
    .unwrap();
    fs::write(site.root.join("dist/report.docx"), "other").unwrap();
    settled_after_a_start(&site, &mut remote, &dist_left_out());
    settled_after_a_start(&site, &mut remote, &SyncRules::default());
    assert_eq!(server_bytes(&remote, &id), b"report");
    assert_eq!(remote.tree().get("dist/kept.docx"), Some(&id));
}

#[test]
fn a_left_out_folder_moved_aside_and_rebuilt_while_stopped_keeps_its_node() {
    let mut remote = Remote::new();
    let (site, b) = moved_into_a_left_out_folder(&mut remote, "dist/report.docx");
    let id = remote.tree()["report.docx"].clone();
    drop(b);
    fs::create_dir(site.root.join("archive")).unwrap();
    fs::rename(site.root.join("dist"), site.root.join("archive/dist")).unwrap();
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::write(site.root.join("dist/report.docx"), "rebuilt").unwrap();
    settled_after_a_start(&site, &mut remote, &dist_left_out());
    fs::rename(
        site.root.join("archive/dist"),
        site.root.join("archive/kept"),
    )
    .unwrap();
    settled_after_a_start(&site, &mut remote, &dist_left_out());
    assert_eq!(server_bytes(&remote, &id), b"report");
    assert_eq!(remote.tree().get("archive/kept/report.docx"), Some(&id));
}

#[test]
fn a_left_out_folder_moved_into_the_synced_tree_and_rebuilt_while_stopped_keeps_its_node() {
    let mut remote = Remote::new();
    let (site, b) = moved_into_a_left_out_folder(&mut remote, "dist/report.docx");
    let id = remote.tree()["report.docx"].clone();
    drop(b);
    fs::rename(site.root.join("dist"), site.root.join("docs")).unwrap();
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::write(site.root.join("dist/report.docx"), "rebuilt").unwrap();
    settled_after_a_start(&site, &mut remote, &dist_left_out());
    assert_eq!(server_bytes(&remote, &id), b"report");
    assert_eq!(remote.tree().get("docs/report.docx"), Some(&id));
}

#[test]
fn a_copy_saved_by_rename_while_stopped_in_a_folder_the_rules_let_in_at_start_keeps_its_node() {
    let mut remote = Remote::new();
    let (site, b) = moved_into_a_left_out_folder(&mut remote, "dist/report.docx");
    let id = remote.tree()["report.docx"].clone();
    drop(b);
    fs::write(site.root.join("dist/.~report.tmp"), "edited").unwrap();
    fs::rename(
        site.root.join("dist/.~report.tmp"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    settled_after_a_start(&site, &mut remote, &SyncRules::default());
    assert_eq!(remote.tree().get("dist/report.docx"), Some(&id));
    assert_eq!(remote.tree().len(), 2, "{:?}", remote.tree());
    assert_eq!(server_bytes(&remote, &id), b"edited");
}

#[test]
fn a_left_out_copy_moved_over_another_ones_found_place_while_stopped_keeps_both_nodes_apart() {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::write(site.root.join("a.docx"), "a").unwrap();
    fs::write(site.root.join("b.docx"), "b").unwrap();
    b.settle();
    b.rest(&mut remote);
    drop(b);
    let mut b = started(&site, &mut remote, &dist_left_out());
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::rename(site.root.join("a.docx"), site.root.join("dist/a.docx")).unwrap();
    fs::rename(site.root.join("b.docx"), site.root.join("dist/b.docx")).unwrap();
    for _ in 0..3 {
        b.settle();
        b.rest(&mut remote);
    }
    let ida = remote.tree()["a.docx"].clone();
    let idb = remote.tree()["b.docx"].clone();
    drop(b);
    fs::rename(site.root.join("dist/b.docx"), site.root.join("dist/a.docx")).unwrap();
    settled_after_a_start(&site, &mut remote, &dist_left_out());
    settled_after_a_start(&site, &mut remote, &SyncRules::default());
    assert!(files(&site.root).contains(&("a.docx".to_owned(), "a".to_owned())));
    assert_eq!(server_bytes(&remote, &ida), b"a");
    assert_eq!(server_bytes(&remote, &idb), b"b");
}

#[test]
fn a_left_out_copy_replaced_by_a_hard_link_of_a_synced_file_while_stopped_downloads_again() {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::write(site.root.join("report.docx"), "report").unwrap();
    fs::write(site.root.join("notes.docx"), "notes").unwrap();
    b.settle();
    b.rest(&mut remote);
    drop(b);
    let mut b = started(&site, &mut remote, &dist_left_out());
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::rename(
        site.root.join("report.docx"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    for _ in 0..3 {
        b.settle();
        b.rest(&mut remote);
    }
    drop(b);
    fs::remove_file(site.root.join("dist/report.docx")).unwrap();
    fs::hard_link(
        site.root.join("notes.docx"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    settled_after_a_start(&site, &mut remote, &dist_left_out());
    settled_after_a_start(&site, &mut remote, &SyncRules::default());
    assert!(
        files(&site.root).contains(&("report.docx".to_owned(), "report".to_owned())),
        "{:?}",
        files(&site.root)
    );
}

#[test]
fn a_synced_file_moved_over_a_left_out_copy_in_a_folder_let_in_at_start_downloads_it_again() {
    let mut remote = Remote::new();
    let (site, b) = moved_into_a_left_out_folder(&mut remote, "dist/report.docx");
    let id = remote.tree()["report.docx"].clone();
    drop(b);
    fs::write(site.root.join("notes.docx"), "notes").unwrap();
    settled_after_a_start(&site, &mut remote, &dist_left_out());
    fs::rename(
        site.root.join("notes.docx"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    settled_after_a_start(&site, &mut remote, &SyncRules::default());
    assert!(
        files(&site.root).contains(&("report.docx".to_owned(), "report".to_owned())),
        "{:?}",
        files(&site.root)
    );
    assert_eq!(server_bytes(&remote, &id), b"report");
}

#[test]
fn a_copy_in_a_left_out_folder_saved_by_rename_stays_the_only_copy() {
    let mut remote = Remote::new();
    let (site, mut b) = moved_into_a_left_out_folder(&mut remote, "dist/report.docx");
    fs::write(site.root.join("dist/.~report.tmp"), "edited").unwrap();
    fs::rename(
        site.root.join("dist/.~report.tmp"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    b.settle();
    b.rest(&mut remote);
    drop(b);
    let b = started(&site, &mut remote, &dist_left_out());
    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert_eq!(
        files(&site.root),
        [("dist/report.docx".to_owned(), "edited".to_owned())]
    );
}

#[test]
fn a_copy_in_a_left_out_folder_kept_only_through_a_hard_link_stays_the_only_copy() {
    let mut remote = Remote::new();
    let (site, mut b) = moved_into_a_left_out_folder(&mut remote, "dist/report.docx");
    fs::hard_link(
        site.root.join("dist/report.docx"),
        site.root.join("dist/link.docx"),
    )
    .unwrap();
    fs::remove_file(site.root.join("dist/report.docx")).unwrap();
    b.settle();
    b.rest(&mut remote);
    drop(b);
    started(&site, &mut remote, &dist_left_out());
    assert_eq!(
        files(&site.root),
        [("dist/link.docx".to_owned(), "report".to_owned())]
    );
}

#[test]
fn a_copy_below_an_unreadable_folder_that_the_rules_let_in_again_is_not_downloaded_again() {
    use std::os::unix::fs::PermissionsExt;
    let mut remote = Remote::new();
    let (site, b) = moved_into_a_left_out_folder(&mut remote, "dist/locked/report.docx");
    drop(b);
    let locked = site.root.join("dist/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let mut b = started(&site, &mut remote, &SyncRules::default());
    b.rest(&mut remote);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    b.settle();
    b.rest(&mut remote);
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert!(!sent.contains(&"delete"), "{sent:?}");
    assert_eq!(
        files(&site.root),
        [("dist/locked/report.docx".to_owned(), "report".to_owned())],
        "{sent:?}"
    );
}

fn a_file_deleted_after_its_folder_moved_out_of_the_rules_is_deleted(rules: &str, to: &str) {
    for victim in ["f0.docx", "sub/f1.docx"] {
        let (mut remote, site, _) = excluded_dist(3);
        let mut b = started(&site, &mut remote, &SyncRules::default());
        if let Some(parent) = Path::new(to)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(site.root.join(parent)).unwrap();
        }
        fs::write(site.root.join(".koiignore"), rules).unwrap();
        b.rest(&mut remote);
        let id = remote.tree()[&format!("dist/{victim}")].clone();
        fs::rename(site.root.join("dist"), site.root.join(to)).unwrap();
        b.rest(&mut remote);
        let before = b.sent.len();
        let path = format!("{to}/{victim}");
        assert_eq!(remote.tree().get(&path), Some(&id), "{path}");
        fs::remove_file(site.root.join(&path)).unwrap();
        b.rest(&mut remote);
        let sent: Vec<&str> = b.sent[before..].iter().flat_map(kinds).collect();
        assert_eq!(sent, ["delete"], "{path}");
        assert!(!site.root.join(&path).exists(), "{path}");
        assert!(
            !remote.tree().values().any(|live| *live == id),
            "{path}: {:?}",
            remote.tree()
        );
    }
}

#[test]
fn a_file_deleted_after_its_folder_is_renamed_out_of_the_rules_is_deleted() {
    a_file_deleted_after_its_folder_moved_out_of_the_rules_is_deleted("dist/\n", "shipped");
}

#[test]
fn a_file_deleted_after_its_folder_is_moved_out_of_the_rules_is_deleted() {
    a_file_deleted_after_its_folder_moved_out_of_the_rules_is_deleted("/dist/\n", "docs/dist");
}

#[test]
fn a_file_deleted_while_its_folder_was_excluded_downloads_again_when_the_folder_is_renamed_out_of_the_rules()
 {
    let (mut remote, site, synced) = excluded_dist(3);
    let mut b = started(&site, &mut remote, &SyncRules::default());
    fs::write(site.root.join(".koiignore"), "dist/\n").unwrap();
    b.rest(&mut remote);
    b.sent.clear();
    fs::remove_file(site.root.join("dist/sub/f1.docx")).unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert!(b.sent.is_empty(), "{:?}", b.sent);

    fs::rename(site.root.join("dist"), site.root.join("shipped")).unwrap();
    b.rest(&mut remote);
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert!(
        !sent.contains(&"delete") && !sent.contains(&"create file"),
        "{sent:?}"
    );
    let local: Vec<(String, String)> = (files(&site.root).into_iter())
        .filter(|(path, _)| path != ".koiignore")
        .collect();
    let moved: Vec<(String, String)> = (synced.into_iter())
        .map(|(path, text)| (path.replacen("dist/", "shipped/", 1), text))
        .collect();
    assert_eq!(local, moved);
    assert!(shown(&site).is_empty());
}

#[test]
fn a_file_removed_while_stopped_with_a_rules_file_written_to_exclude_it_downloads_again_when_the_rule_is_lifted()
 {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &site.root.join("report.docx"), b"report");
    b.create(&mut remote, &site.root.join(".koiignore"), b"other.txt\n");
    drop(b);
    fs::remove_file(site.root.join("report.docx")).unwrap();
    fs::write(site.root.join(".koiignore"), b"report.docx\n").unwrap();

    let mut b = started(&site, &mut remote, &SyncRules::default());
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert_eq!(sent, ["update content"]);
    assert_eq!(remote.names(), [".koiignore", "report.docx"]);

    fs::write(site.root.join(".koiignore"), b"other.txt\n").unwrap();
    b.rescan();
    b.rest(&mut remote);
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert_eq!(sent, ["update content", "update content"]);
    assert_eq!(fs::read(site.root.join("report.docx")).unwrap(), b"report");
    assert_eq!(remote.names(), [".koiignore", "report.docx"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

struct Unreadable<'a>(&'a Path);

impl Unreadable<'_> {
    fn new(folder: &Path) -> Unreadable<'_> {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(folder, fs::Permissions::from_mode(0o000)).unwrap();
        Unreadable(folder)
    }
}

impl Drop for Unreadable<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn a_file_removed_while_left_out_below_a_folder_the_first_scan_after_a_restart_cannot_read_stays_the_server_file()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let folder = site.root.join("private");
    fs::create_dir(&folder).unwrap();
    a.create(&mut remote, &folder.join("report.docx"), b"report");
    a.create(&mut remote, &folder.join(".koiignore"), b"report.docx\n");
    a.rest(&mut remote);
    fs::remove_file(folder.join("report.docx")).unwrap();
    a.rescan();
    a.rest(&mut remote);
    let id = node_named(&a, "report.docx");
    drop(a);

    let unreadable = Unreadable::new(&folder);
    if fs::read_dir(&folder).is_ok() {
        assert!(rustix::process::geteuid().is_root());
        eprintln!("SKIPPED: running as root, so permissions do not block reads");
        return;
    }
    fs::write(site.root.join("notes.txt"), b"notes").unwrap();
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    drop(unreadable);
    a.rescan();
    a.rest(&mut remote);
    let before = a.sent.len();
    fs::write(folder.join("report.docx"), b"rebuilt").unwrap();
    a.rescan();
    a.rest(&mut remote);
    fs::write(folder.join(".koiignore"), b"other.txt\n").unwrap();
    a.rescan();
    a.rest(&mut remote);
    let sent: Vec<&str> = a.sent[before..].iter().flat_map(kinds).collect();
    assert_eq!(sent, ["update content", "update content"]);
    assert_eq!(node_named(&a, "report.docx"), id);
    assert_eq!(
        remote.names(),
        [".koiignore", "notes.txt", "private", "report.docx"]
    );
    assert_eq!(
        files(&site.root),
        [
            ("notes.txt".into(), "notes".into()),
            ("private/.koiignore".into(), "other.txt\n".into()),
            ("private/report.docx".into(), "rebuilt".into()),
        ]
    );
    assert!(shown(&site).is_empty());
}

#[test]
fn a_file_removed_while_left_out_below_a_folder_a_restart_cannot_read_downloads_once_a_scan_reads_the_folder()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let folder = site.root.join("private");
    fs::create_dir(&folder).unwrap();
    a.create(&mut remote, &folder.join("a.log"), b"log");
    a.create(&mut remote, &site.root.join(".koiignore"), b"*.log\n");
    a.rest(&mut remote);
    fs::remove_file(folder.join("a.log")).unwrap();
    a.rescan();
    a.rest(&mut remote);
    drop(a);

    let unreadable = Unreadable::new(&folder);
    if fs::read_dir(&folder).is_ok() {
        assert!(rustix::process::geteuid().is_root());
        eprintln!("SKIPPED: running as root, so permissions do not block reads");
        return;
    }
    fs::write(site.root.join(".koiignore"), b"other.txt\n").unwrap();
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    drop(unreadable);
    assert!(!folder.join("a.log").exists());
    a.rescan();
    a.rest(&mut remote);
    assert_eq!(fs::read(folder.join("a.log")).unwrap(), b"log");
    let sent: Vec<&str> = a.sent.iter().flat_map(kinds).collect();
    assert!(!sent.contains(&"delete"), "{sent:?}");
    assert!(shown(&site).is_empty());
}

#[test]
fn a_file_deleted_where_the_rules_leave_it_out_downloads_again_once_another_device_moves_its_folder_into_the_rules()
 {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::write(site.root.join("dist/report.docx"), "report").unwrap();
    b.rescan();
    b.rest(&mut remote);
    drop(b);
    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["dist/report.docx".to_owned()],
        ..SyncRules::default()
    };
    let mut b = started(&site, &mut remote, &sync);
    fs::remove_file(site.root.join("dist/report.docx")).unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert!(b.sent.is_empty(), "{:?}", b.sent);

    let (site_a, mut a) = linked(&mut remote, "a");
    a.rest(&mut remote);
    fs::rename(site_a.root.join("dist"), site_a.root.join("out")).unwrap();
    a.rescan();
    a.rest(&mut remote);
    b.pull(&remote);
    b.rest(&mut remote);
    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert!(shown(&site).is_empty());
    assert_eq!(
        files(&site.root),
        [("out/report.docx".to_owned(), "report".to_owned())]
    );
}

#[test]
fn a_file_deleted_while_another_device_kept_it_in_an_excluded_folder_comes_back_beside_a_name_sharing_its_key()
 {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("keep")).unwrap();
    a.create(&mut remote, &site_a.root.join("keep/ss"), b"ss");
    let site = Site::new(&remote, "b");
    let mut b = started(&site, &mut remote, &dist_left_out());
    assert_eq!(files(&site.root), [("keep/ss".to_owned(), "ss".to_owned())]);

    fs::create_dir(site_a.root.join("dist")).unwrap();
    fs::rename(site_a.root.join("keep/ss"), site_a.root.join("dist/ss")).unwrap();
    a.rescan();
    a.rest(&mut remote);
    b.pull(&remote);
    b.rest(&mut remote);
    for left in ["keep/ss", "dist/ss"] {
        if site.root.join(left).exists() {
            fs::remove_file(site.root.join(left)).unwrap();
        }
    }
    fs::write(site.root.join("keep/\u{df}"), "eszett").unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert!(
        remote.tree().contains_key("keep/\u{df}"),
        "{:?}",
        remote.tree()
    );

    fs::rename(site_a.root.join("dist/ss"), site_a.root.join("keep/ss")).unwrap();
    a.rescan();
    a.rest(&mut remote);
    b.pull(&remote);
    b.rest(&mut remote);
    let local = files(&site.root);
    let mut texts: Vec<&str> = local.iter().map(|(_, text)| text.as_str()).collect();
    texts.sort();
    assert_eq!(texts, ["eszett", "ss"], "{local:?}");
    assert!(
        local.iter().any(|(path, _)| path.contains("conflict")),
        "{local:?}"
    );
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert!(!sent.contains(&"delete"), "sent {sent:?}");
    assert!(shown(&site).is_empty());
}

#[test]
fn a_renamed_folder_removed_while_stopped_with_a_rule_that_leaves_it_out_downloads_again() {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site.root.join("old")).unwrap();
    b.create(&mut remote, &site.root.join("old/f.docx"), b"report");
    b.rest(&mut remote);
    let id = node_named(&b, "f.docx");
    fs::rename(site.root.join("old"), site.root.join("new")).unwrap();
    b.scan_until(|b| {
        (b.folder.engine.trees.local().iter()).any(|(_, node)| node.shape.name == "new")
    });
    drop(b);
    fs::write(site.root.join(".koiignore"), "/new/\n").unwrap();
    fs::remove_dir_all(site.root.join("new")).unwrap();
    let mut b = restarted(&site, &mut remote);
    b.scan_until(|b| b.folder.engine.trees.local().get(&id).is_none());
    b.rest(&mut remote);
    fs::write(site.root.join(".koiignore"), "").unwrap();
    b.rescan();
    b.rest(&mut remote);
    let deletes: Vec<&Op> = (b.sent.iter())
        .flat_map(|commit| &commit.ops)
        .filter(|op| matches!(op.op, Some(op::Op::Delete(_))))
        .collect();
    assert!(deletes.is_empty(), "sent {deletes:?}");
    assert_eq!(
        files(&site.root),
        [
            (".koiignore".into(), "".into()),
            ("new/f.docx".into(), "report".into())
        ]
    );
}

#[test]
fn a_file_removed_while_excluded_before_its_create_is_acknowledged_downloads_again_once_let_in() {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::write(site.root.join("report.docx"), b"report").unwrap();
    b.rescan();
    b.scan_until(|b| !b.work.is_empty());
    let create = loop {
        let work = b.work.pop_front().expect("the create is sent");
        if matches!(work, Work::Send(_)) {
            break work;
        }
        b.perform(work, &mut remote).unwrap();
    };
    fs::write(site.root.join(".koiignore"), b"report.docx\n").unwrap();
    b.rescan();
    b.settle();
    fs::remove_file(site.root.join("report.docx")).unwrap();
    b.rescan();
    b.settle();
    b.perform(create, &mut remote).unwrap();
    b.rest(&mut remote);
    let before = b.sent.len();
    fs::write(site.root.join(".koiignore"), b"other.txt\n").unwrap();
    b.rescan();
    b.rest(&mut remote);
    let sent: Vec<&str> = b.sent[before..].iter().flat_map(kinds).collect();
    assert_eq!(sent, ["update content"]);
    assert!(remote.tree().contains_key("report.docx"));
    assert_eq!(fs::read(site.root.join("report.docx")).unwrap(), b"report");
}

#[test]
fn a_copy_rebuilt_while_excluded_whose_content_a_later_batch_reads_takes_back_its_node() {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &site.root.join("zzz.txt"), b"one");
    b.rest(&mut remote);
    let id = node_named(&b, "zzz.txt");
    fs::write(site.root.join(".koiignore"), b"zzz.txt\n").unwrap();
    b.rescan();
    b.rest(&mut remote);
    fs::remove_file(site.root.join("zzz.txt")).unwrap();
    b.rescan();
    b.rest(&mut remote);
    for n in 0..crate::observe::READ_FILES {
        fs::write(site.root.join(format!("f{n:04}.txt")), b"f").unwrap();
    }
    fs::write(site.root.join("zzz.txt"), b"rebuilt").unwrap();
    fs::write(site.root.join(".koiignore"), b"other.txt\n").unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert_eq!(remote.tree().get("zzz.txt"), Some(&id.to_string()));
    assert_eq!(fs::read(site.root.join("zzz.txt")).unwrap(), b"rebuilt");
    assert_eq!(files(&site.root).len(), crate::observe::READ_FILES + 2);
    assert!(shown(&site).is_empty());
}

#[test]
fn a_file_read_in_a_later_batch_downloads_again_once_removed_while_excluded() {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &site.root.join("zzz.txt"), b"one");
    b.rest(&mut remote);
    for n in 0..crate::observe::READ_FILES {
        fs::write(site.root.join(format!("f{n:04}.txt")), b"f").unwrap();
    }
    fs::write(site.root.join("zzz.txt"), b"two").unwrap();
    b.rescan();
    b.rest(&mut remote);
    fs::write(site.root.join(".koiignore"), b"zzz.txt\n").unwrap();
    b.rescan();
    b.rest(&mut remote);
    fs::remove_file(site.root.join("zzz.txt")).unwrap();
    b.rescan();
    b.rest(&mut remote);
    fs::write(site.root.join(".koiignore"), b"other.txt\n").unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert_eq!(fs::read(site.root.join("zzz.txt")).unwrap(), b"two");
}

#[test]
fn a_copy_rebuilt_while_excluded_takes_back_its_node_when_another_device_moves_it_into_the_rules() {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::write(site.root.join("dist/report.docx"), "report").unwrap();
    b.rescan();
    b.rest(&mut remote);
    let report = remote.tree()["dist/report.docx"].clone();
    let (asite, mut a) = linked(&mut remote, "a");
    a.pull(&remote);
    a.rest(&mut remote);
    fs::write(site.root.join(".koiignore"), "dist/report.docx\n").unwrap();
    b.rest(&mut remote);
    fs::remove_file(site.root.join("dist/report.docx")).unwrap();
    b.rest(&mut remote);
    fs::write(site.root.join("dist/report.docx"), "rebuilt").unwrap();
    b.rest(&mut remote);
    a.pull(&remote);
    a.rest(&mut remote);
    fs::rename(asite.root.join("dist"), asite.root.join("out")).unwrap();
    a.rescan();
    a.rest(&mut remote);
    b.pull(&remote);
    b.rest(&mut remote);
    b.rescan();
    b.pull(&remote);
    b.rest(&mut remote);
    assert_eq!(remote.tree().get("out/report.docx"), Some(&report));
    let expected = [
        (".koiignore", "dist/report.docx\n"),
        ("out/report.docx", "rebuilt"),
    ];
    assert_eq!(
        files(&site.root),
        expected.map(|(path, bytes)| (path.to_owned(), bytes.to_owned()))
    );
    assert!(shown(&site).is_empty());
}
#[cfg(target_os = "linux")]
#[test]
fn a_file_moved_into_a_left_out_folder_the_search_cannot_open_stays_on_the_server_under_a_question()
{
    use std::os::unix::fs::PermissionsExt;

    for (target, rescan) in [
        ("dist/locked/report.docx", true),
        ("dist/locked/inner/report.docx", true),
        ("dist/locked/report.docx", false),
    ] {
        let case = format!("{target} rescan={rescan}");
        let mut remote = Remote::new();
        let (site, mut b) = linked(&mut remote, "b");
        fs::write(site.root.join("report.docx"), "report").unwrap();
        b.rescan();
        b.rest(&mut remote);
        drop(b);
        let mut b = started(&site, &mut remote, &dist_left_out());
        fs::create_dir_all(site.root.join(target).parent().unwrap()).unwrap();
        b.rest(&mut remote);
        fs::rename(site.root.join("report.docx"), site.root.join(target)).unwrap();
        let locked = site.root.join("dist/locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&locked).is_ok() {
            assert!(rustix::process::geteuid().is_root());
            eprintln!("SKIPPED: running as root, so permissions do not block reads");
            return;
        }
        if rescan {
            b.rescan();
        }
        for _ in 0..50 {
            b.run(&mut remote);
            let _ = b.try_scan(Duration::from_millis(100));
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
        assert!(!sent.contains(&"delete"), "{case}: sent {sent:?}");
        assert_eq!(remote.names(), ["report.docx"], "{case}");
        let questions: Vec<_> = (shown(&site).into_iter())
            .map(|question| {
                let items: Vec<String> = (question.items.iter())
                    .map(|item| item.path.unquoted().to_owned())
                    .collect();
                (question.kind, items)
            })
            .collect();
        let held = (
            crate::question::QuestionKind::AmbiguousDisappearance,
            vec!["report.docx".to_owned()],
        );
        assert_eq!(questions, [held], "{case}");
    }
}
#[cfg(target_os = "linux")]
#[test]
fn a_held_file_after_a_server_rewind_stays_under_its_one_question() {
    use std::os::unix::fs::PermissionsExt;
    fn spin(b: &mut Device, remote: &mut Remote) {
        for _ in 0..20 {
            let _ = b.try_scan(Duration::from_millis(50));
            b.run(remote);
        }
    }
    for (past_create, order, given) in [
        (false, [true, false], Answer::Delete),
        (false, [false, true], Answer::Delete),
        (true, [true, false], Answer::Delete),
        (false, [true, false], Answer::Restore),
    ] {
        let case = format!("past_create={past_create} order={order:?} {given}");
        let mut remote = Remote::new();
        let (site, mut b) = linked(&mut remote, "b");
        let mut backup = remote.space.clone();
        fs::write(site.root.join("report.docx"), "v1").unwrap();
        b.rescan();
        b.rest(&mut remote);
        if !past_create {
            backup = remote.space.clone();
        }
        fs::write(site.root.join("report.docx"), "v2 edited").unwrap();
        b.rescan();
        b.rest(&mut remote);
        drop(b);
        let mut b = started(&site, &mut remote, &dist_left_out());
        let locked = site.root.join("dist/locked");
        fs::create_dir_all(&locked).unwrap();
        b.rest(&mut remote);
        fs::rename(site.root.join("report.docx"), locked.join("report.docx")).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&locked).is_ok() {
            assert!(rustix::process::geteuid().is_root());
            eprintln!("SKIPPED: running as root, so permissions do not block reads");
            return;
        }
        b.rescan();
        spin(&mut b, &mut remote);

        remote.space.rewind(&backup, NOW);
        b.event(Event::Connected(remote.space.welcome(&b.caller)))
            .unwrap();
        for _ in 0..3 {
            b.pull(&remote);
            spin(&mut b, &mut remote);
            b.settle();
        }
        let asked: Vec<_> = (shown(&site).into_iter())
            .map(|question| question.kind)
            .collect();
        for ambiguous in order {
            let mut store = Store::open(&site.db).unwrap();
            for row in store.transaction(|tx| tx.questions()).unwrap() {
                let question: crate::question::Question =
                    serde_json::from_slice(&row.body).unwrap();
                if (question.kind == crate::question::QuestionKind::AmbiguousDisappearance)
                    == ambiguous
                {
                    let answer = koidrive_store::GivenAnswer {
                        answer: given.as_str().into(),
                        answered_at_ms: NOW,
                    };
                    (store.transaction(|tx| tx.answer_question(&row.question_id, &answer)))
                        .unwrap();
                }
            }
            let step = b.folder.requested();
            b.take(step).unwrap();
            spin(&mut b, &mut remote);
            b.settle();
            spin(&mut b, &mut remote);
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let restored = given == Answer::Restore;
        let names: &[&str] = if restored { &["report.docx"] } else { &[] };
        assert_eq!(remote.names(), names, "{case}");
        assert_eq!(site.root.join("report.docx").exists(), restored, "{case}");
        assert!(locked.join("report.docx").exists(), "{case}");
        let expected: &[_] = match past_create {
            true => &[],
            false => &[crate::question::QuestionKind::AmbiguousDisappearance],
        };
        assert_eq!(asked, expected, "{case}");
        assert_eq!(shown(&site).len(), 0, "{case}");
    }
}

#[test]
fn a_synced_folder_deleted_in_an_ignored_folder_while_stopped_comes_back_and_a_file_still_there_stays()
 {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site.root.join("album")).unwrap();
    fs::write(site.root.join("album/a.jpg"), "a").unwrap();
    fs::write(site.root.join("kept.docx"), "kept").unwrap();
    b.rescan();
    b.rest(&mut remote);
    let server = remote.tree();
    drop(b);
    let mut b = started(&site, &mut remote, &dist_left_out());
    fs::create_dir_all(site.root.join("dist/deep")).unwrap();
    fs::rename(site.root.join("album"), site.root.join("dist/deep/album")).unwrap();
    fs::rename(
        site.root.join("kept.docx"),
        site.root.join("dist/kept.docx"),
    )
    .unwrap();
    b.rescan();
    b.rest(&mut remote);
    drop(b);
    fs::remove_dir_all(site.root.join("dist/deep/album")).unwrap();

    let b = started(&site, &mut remote, &dist_left_out());
    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert_eq!(remote.tree(), server);
    let expected = [("album/a.jpg", "a"), ("dist/kept.docx", "kept")];
    assert_eq!(
        files(&site.root),
        expected.map(|(path, bytes)| (path.to_owned(), bytes.to_owned()))
    );
}

#[test]
fn a_copy_below_an_unreadable_left_out_folder_is_not_downloaded_again() {
    use std::os::unix::fs::PermissionsExt;
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::write(site.root.join("report.docx"), "report").unwrap();
    b.rescan();
    b.rest(&mut remote);
    drop(b);
    let mut b = started(&site, &mut remote, &dist_left_out());
    fs::create_dir_all(site.root.join("dist/locked")).unwrap();
    fs::rename(
        site.root.join("report.docx"),
        site.root.join("dist/locked/report.docx"),
    )
    .unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert!(b.sent.is_empty());
    drop(b);
    let locked = site.root.join("dist/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let b = started(&site, &mut remote, &dist_left_out());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert!(!sent.contains(&"delete"), "{sent:?}");
    assert_eq!(
        files(&site.root),
        [("dist/locked/report.docx".to_owned(), "report".to_owned())],
        "{sent:?}"
    );
}

#[test]
fn a_copy_below_a_left_out_folder_that_turns_readable_again_comes_back_only_if_it_was_deleted() {
    use std::os::unix::fs::PermissionsExt;
    for deleted in [false, true] {
        let mut remote = Remote::new();
        let (site, mut b) = linked(&mut remote, "b");
        fs::write(site.root.join("report.docx"), "report").unwrap();
        b.rescan();
        b.rest(&mut remote);
        drop(b);
        let mut b = started(&site, &mut remote, &dist_left_out());
        fs::create_dir_all(site.root.join("dist/locked")).unwrap();
        let copy = site.root.join("dist/locked/report.docx");
        fs::rename(site.root.join("report.docx"), &copy).unwrap();
        b.rescan();
        b.rest(&mut remote);
        let locked = site.root.join("dist/locked");
        let mode = |mode| fs::set_permissions(&locked, fs::Permissions::from_mode(mode)).unwrap();
        mode(0o000);
        if fs::read_dir(&locked).is_ok() {
            assert!(rustix::process::geteuid().is_root());
            eprintln!("SKIPPED: running as root, so permissions do not block reads");
            return;
        }
        b.rescan();
        b.rest(&mut remote);
        if deleted {
            mode(0o755);
            fs::remove_file(&copy).unwrap();
            mode(0o000);
            b.rescan();
            b.rest(&mut remote);
        }
        assert!(
            !site.root.join("report.docx").exists(),
            "deleted: {deleted}"
        );
        mode(0o755);
        b.rest(&mut remote);
        let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
        assert!(!sent.contains(&"delete"), "deleted: {deleted}: {sent:?}");
        let expected = match deleted {
            false => ("dist/locked/report.docx", "report"),
            true => ("report.docx", "report"),
        };
        assert_eq!(
            files(&site.root),
            [(expected.0.to_owned(), expected.1.to_owned())],
            "deleted: {deleted}"
        );
    }
}
