// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_folder_swapped_for_one_holding_its_entries_is_reidentified() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    fs::create_dir_all(root.join("photos")).unwrap();
    fs::write(root.join("photos/one.jpeg"), "one").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    let before = snapshot.entries[Path::new("photos")].id;

    fs::create_dir(root.join("photos.new")).unwrap();
    fs::rename(
        root.join("photos/one.jpeg"),
        root.join("photos.new/one.jpeg"),
    )
    .unwrap();
    fs::remove_dir(root.join("photos")).unwrap();
    fs::rename(root.join("photos.new"), root.join("photos")).unwrap();
    let rescan = snapshot.rescan(&root, [PathBuf::new()]).unwrap();

    let after = snapshot.entries[Path::new("photos")].id;
    assert_ne!(after, before);
    assert_eq!(
        rescan.changes,
        [Change::Reidentified {
            path: PathBuf::from("photos")
        }]
    );
}

#[test]
fn a_folder_swapped_for_one_holding_its_entries_is_reidentified_by_a_rescan_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    fs::create_dir_all(root.join("photos")).unwrap();
    fs::write(root.join("photos/one.jpeg"), "one").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();

    fs::create_dir(root.join("photos.new")).unwrap();
    fs::rename(
        root.join("photos/one.jpeg"),
        root.join("photos.new/one.jpeg"),
    )
    .unwrap();
    fs::remove_dir(root.join("photos")).unwrap();
    fs::rename(root.join("photos.new"), root.join("photos")).unwrap();
    let rescan = snapshot
        .rescan(
            &root,
            [PathBuf::from("photos"), PathBuf::from("photos.new")],
        )
        .unwrap();

    assert_eq!(
        rescan.changes,
        [Change::Reidentified {
            path: PathBuf::from("photos")
        }]
    );
}

#[cfg(target_vendor = "apple")]
#[test]
fn a_folder_swapped_for_another_volume_while_it_opens_is_not_published() {
    let mut disks = koidrive_test_support::Disks::new().unwrap();
    let root = disks.base().join("root");
    let nested = root.join("nested");
    let elsewhere = disks.base().join("elsewhere");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&nested).unwrap();
    fs::create_dir(&elsewhere).unwrap();
    let a = disks.image("a");
    let b = disks.image("b");
    disks.attach(&b, &elsewhere);
    fs::write(elsewhere.join("impostor.txt"), b"other volume").unwrap();
    disks.detach(&elsewhere);
    disks.attach(&a, &nested);
    fs::write(nested.join("original.txt"), b"original volume").unwrap();
    let before = stat_path(&nested).unwrap();
    let mut snapshot = Snapshot::default();
    let mut swapped = false;
    let result = snapshot.rescan_with(&root, [PathBuf::new()], None, &mut |cut| {
        if matches!(cut, Cut::OpeningDir(path) if path == Path::new("nested")) && !swapped {
            disks.unmount(&nested);
            disks.attach(&b, &nested);
            swapped = true;
        }
    });
    let after = stat_path(&nested).unwrap();
    assert!(swapped);
    assert_eq!(before.id.ino, after.id.ino);
    assert_ne!(before.dev, after.dev);
    assert!(matches!(result, Err(Error::Changed)), "{result:?}");
    assert!(snapshot.entries.is_empty(), "{:?}", snapshot.entries);
}

#[test]
fn a_scan_counts_the_files_it_finds_as_it_walks() {
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("a/b")).unwrap();
    for file in ["one.txt", "a/two.txt", "a/b/three.txt"] {
        fs::write(root.join(file), file).unwrap();
    }
    let counts = Arc::new(Mutex::new(Vec::new()));
    let native = counts.clone();
    let mut watcher = NativeWatcher::start(&root, Snapshot::default())
        .unwrap()
        .counting(move |files| native.lock().unwrap().push(files));
    watcher.poll(Duration::ZERO).unwrap();
    assert_eq!(*counts.lock().unwrap(), [1, 2, 3]);

    let rescanned = Arc::new(Mutex::new(Vec::new()));
    let rescan = rescanned.clone();
    let mut watcher = RescanWatcher::new(&root, Snapshot::default(), Duration::ZERO)
        .unwrap()
        .counting(move |files| rescan.lock().unwrap().push(files));
    watcher.poll(Duration::ZERO).unwrap();
    watcher.poll(Duration::ZERO).unwrap();
    assert_eq!(*rescanned.lock().unwrap(), [1, 2, 3, 1, 2, 3]);
}

fn scan_settling(snapshot: &mut Snapshot, root: &Path, scope: &str) -> Rescan {
    snapshot
        .rescan_under(
            root,
            [PathBuf::from(scope)],
            &BTreeSet::new(),
            None,
            None,
            &|| false,
            &|_| {},
            Keep::Unexplained {
                waiting: &BTreeSet::new(),
            },
            &mut |_| {},
            &MarkLog::default(),
        )
        .unwrap()
}

fn one_file() -> (tempfile::TempDir, PathBuf, Snapshot) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("x")).unwrap();
    fs::write(root.join("x/g.txt"), "old").unwrap();
    let snapshot = Snapshot::scan(&root).unwrap();
    (dir, root, snapshot)
}

#[test]
fn a_file_replaced_at_its_path_after_a_scan_kept_its_removal_is_modified() {
    let (_dir, root, mut snapshot) = one_file();
    let _old = fs::File::open(root.join("x/g.txt")).unwrap();
    fs::remove_file(root.join("x/g.txt")).unwrap();
    let first = scan_settling(&mut snapshot, &root, "x");
    assert_eq!(first.changes, vec![]);
    assert_eq!(first.unsettled, vec![PathBuf::from("x/g.txt")]);
    assert!(snapshot.entries.contains_key(Path::new("x/g.txt")));

    fs::write(root.join("x/g.txt"), "new").unwrap();
    let second = scan_settling(&mut snapshot, &root, "x");
    assert_eq!(
        second.changes,
        vec![Change::Modified {
            path: "x/g.txt".into()
        }]
    );
    assert_eq!(snapshot.unplaced, BTreeSet::new());
}

#[test]
fn a_scan_that_keeps_a_removal_reports_the_rest_and_the_next_scan_reports_the_removal() {
    let (_dir, root, mut snapshot) = one_file();
    fs::write(root.join("x/h.txt"), "h").unwrap();
    let _old = fs::File::open(root.join("x/g.txt")).unwrap();
    fs::remove_file(root.join("x/g.txt")).unwrap();
    let first = scan_settling(&mut snapshot, &root, "x");
    assert_eq!(
        first.changes,
        vec![Change::Created {
            path: "x/h.txt".into(),
            kind: EntryKind::File
        }]
    );
    assert_eq!(first.unsettled, vec![PathBuf::from("x/g.txt")]);

    fs::write(root.join("x/i.txt"), "i").unwrap();
    fs::remove_file(root.join("x/h.txt")).unwrap();
    let second = scan_settling(&mut snapshot, &root, "x");
    assert_eq!(
        second.changes,
        vec![
            Change::Removed {
                path: "x/g.txt".into(),
                kind: EntryKind::File
            },
            Change::Created {
                path: "x/i.txt".into(),
                kind: EntryKind::File
            }
        ]
    );
    assert_eq!(second.unsettled, vec![PathBuf::from("x/h.txt")]);
    assert!(!snapshot.entries.contains_key(Path::new("x/g.txt")));
}

fn quiet_watcher(root: &Path, dirty: &[&str]) -> NativeWatcher {
    let mut watcher = NativeWatcher::start(root, Snapshot::scan(root).unwrap()).unwrap();
    (watcher.sender, watcher.events) = mpsc::channel();
    watcher.dirty = dirty.iter().map(PathBuf::from).collect();
    watcher
}

#[test]
fn a_watcher_rescans_a_kept_removal_with_the_paths_of_later_events() {
    let (_dir, root, _) = one_file();
    fs::create_dir(root.join("y")).unwrap();
    let mut watcher = quiet_watcher(&root, &["x/g.txt"]);
    let old = fs::File::open(root.join("x/g.txt")).unwrap();
    fs::remove_file(root.join("x/g.txt")).unwrap();
    let kept = watcher.poll(Duration::ZERO).unwrap();
    assert_eq!(kept.changes, []);
    assert_eq!(kept.pending, [PathBuf::from("x/g.txt")]);

    fs::write(root.join("y/h.txt"), "h").unwrap();
    watcher.dirty.insert(PathBuf::from("y/h.txt"));
    drop(old);
    let changes = watcher.poll(Duration::ZERO).unwrap().changes;
    assert_eq!(
        changes,
        [
            Change::Removed {
                path: "x/g.txt".into(),
                kind: EntryKind::File
            },
            Change::Created {
                path: "y/h.txt".into(),
                kind: EntryKind::File
            }
        ]
    );
}

#[test]
fn a_removal_during_every_poll_holds_back_neither_other_changes_nor_earlier_removals() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("z/victims")).unwrap();
    fs::write(root.join("a/sentinel"), "before").unwrap();
    for i in 0..8 {
        fs::write(root.join(format!("z/victims/{i}")), "existing").unwrap();
    }
    let during = root.clone();
    let scans = std::sync::atomic::AtomicUsize::new(1);
    let mut watcher = quiet_watcher(&root, &["a/sentinel", "z/victims"]).counting(move |count| {
        let i = scans.fetch_add(
            usize::from(count == 1),
            std::sync::atomic::Ordering::Relaxed,
        );
        if count == 1 && i < 8 {
            fs::remove_file(during.join(format!("z/victims/{i}"))).unwrap();
        }
    });
    fs::write(root.join("a/sentinel"), "changed bytes").unwrap();
    fs::remove_file(root.join("z/victims/0")).unwrap();

    let first = watcher.poll(Duration::ZERO).unwrap();
    assert!(first.changes.contains(&Change::Modified {
        path: "a/sentinel".into()
    }));
    let reported: Vec<Change> = (0..2)
        .flat_map(|_| watcher.poll(Duration::ZERO).unwrap().changes)
        .chain(first.changes)
        .collect();
    assert!(
        reported.contains(&Change::Removed {
            path: "z/victims/0".into(),
            kind: EntryKind::File
        }),
        "{reported:?}"
    );
}

#[test]
fn a_move_whose_destination_the_watcher_hears_of_a_poll_later_is_a_rename() {
    for timeout in [Duration::ZERO, Duration::from_secs(1)] {
        for linked in [false, true] {
            a_move_heard_of_a_poll_later(timeout, linked);
        }
    }
}

fn a_move_heard_of_a_poll_later(timeout: Duration, linked: bool) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("x/d")).unwrap();
    fs::create_dir_all(root.join("e")).unwrap();
    fs::write(root.join("x/d/g.txt"), "g").unwrap();
    if linked {
        fs::hard_link(root.join("x/d/g.txt"), root.join("x/alias.txt")).unwrap();
    }
    let mut watcher = quiet_watcher(&root, &["x"]);
    fs::rename(root.join("x/d/g.txt"), root.join("e/moved.txt")).unwrap();
    fs::remove_dir(root.join("x/d")).unwrap();
    let departure = watcher.poll(timeout).unwrap();
    watcher.dirty.insert("e".into());
    let arrival = watcher.poll(timeout).unwrap();
    let changes: Vec<Change> = (departure.changes.into_iter())
        .chain(arrival.changes)
        .chain(watcher.poll(timeout).unwrap().changes)
        .collect();
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert!(
        changes.contains(&Change::Renamed {
            from: "x/d/g.txt".into(),
            to: "e/moved.txt".into(),
            kind: EntryKind::File
        }),
        "{changes:?}"
    );
    assert!(
        changes.contains(&Change::Removed {
            path: "x/d".into(),
            kind: EntryKind::Dir
        }),
        "{changes:?}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_file_moved_away_from_a_link_the_rules_leave_out_is_a_rename() {
    for first_link in ["x/d/g.txt", "ig/g.txt"] {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for part in ["x/d", "e", "ig"] {
            fs::create_dir_all(root.join(part)).unwrap();
        }
        fs::write(root.join(Source::Koiignore.file_name()), "ig\n").unwrap();
        let other = if first_link == "x/d/g.txt" {
            "ig/g.txt"
        } else {
            "x/d/g.txt"
        };
        fs::write(root.join(first_link), "g").unwrap();
        fs::hard_link(root.join(first_link), root.join(other)).unwrap();
        let mut rules = Rules::new(Box::new(Lines::default())).with_search_budget(usize::MAX);
        let mut snapshot = Snapshot::scan_with_rules(&root, &mut rules).unwrap();
        fs::rename(root.join("x/d/g.txt"), root.join("e/moved.txt")).unwrap();
        fs::remove_dir(root.join("x/d")).unwrap();
        let from = (snapshot.rescan_with_rules(&root, ["x".into()], &mut rules)).unwrap();
        let to = (snapshot.rescan_with_rules(&root, ["e".into()], &mut rules)).unwrap();
        let changes: Vec<&Change> = from.changes.iter().chain(&to.changes).collect();
        assert!(
            changes.contains(&&Change::Renamed {
                from: "x/d/g.txt".into(),
                to: "e/moved.txt".into(),
                kind: EntryKind::File
            }),
            "created {first_link} first: {changes:?}"
        );
    }
}

#[test]
fn a_move_out_of_a_folder_a_file_then_replaces_is_a_rename_a_poll_later() {
    for timeout in [Duration::ZERO, Duration::from_secs(1)] {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("x/d")).unwrap();
        fs::create_dir_all(root.join("e")).unwrap();
        fs::write(root.join("x/d/g.txt"), "g").unwrap();
        let mut watcher = quiet_watcher(&root, &["x"]);
        fs::rename(root.join("x/d/g.txt"), root.join("e/moved.txt")).unwrap();
        fs::remove_dir(root.join("x/d")).unwrap();
        fs::write(root.join("x/d"), "a file now").unwrap();
        let departure = watcher.poll(timeout).unwrap();
        watcher.dirty.insert("e".into());
        let arrival = watcher.poll(timeout).unwrap();
        let mut changes: Vec<Change> = (departure.changes.into_iter())
            .chain(arrival.changes)
            .chain(watcher.poll(timeout).unwrap().changes)
            .collect();
        changes.sort_by_key(|change| format!("{change:?}"));
        assert_eq!(
            changes,
            vec![
                Change::Created {
                    path: "x/d".into(),
                    kind: EntryKind::File
                },
                Change::Removed {
                    path: "x/d".into(),
                    kind: EntryKind::Dir
                },
                Change::Renamed {
                    from: "x/d/g.txt".into(),
                    to: "e/moved.txt".into(),
                    kind: EntryKind::File
                },
            ],
            "{timeout:?}"
        );
        assert_eq!(
            watcher.snapshot().entries,
            Snapshot::scan(&root).unwrap().entries
        );
    }
}

#[test]
fn a_held_removal_under_a_folder_unreadable_for_a_poll_is_reported_once_the_folder_can_be_read() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("x/d")).unwrap();
    fs::write(root.join("x/d/g.txt"), "g").unwrap();
    let mut watcher = quiet_watcher(&root, &["x"]);
    fs::remove_dir_all(root.join("x/d")).unwrap();
    assert_eq!(watcher.poll(Duration::ZERO).unwrap().changes, vec![]);
    fs::set_permissions(root.join("x"), fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = blocks_reads(
        "a_held_removal_under_a_folder_unreadable_for_a_poll_is_reported_once_the_folder_can_be_read",
        fs::read_dir(root.join("x")),
    );
    let while_blocked = blocked.then(|| {
        watcher.dirty.insert("x".into());
        watcher.poll(Duration::ZERO)
    });
    fs::set_permissions(root.join("x"), fs::Permissions::from_mode(0o755)).unwrap();
    let Some(while_blocked) = while_blocked else {
        return;
    };
    assert_eq!(while_blocked.unwrap().changes, vec![]);
    watcher.dirty.insert("x".into());
    assert_eq!(
        watcher.poll(Duration::ZERO).unwrap().changes,
        vec![Change::Removed {
            path: "x/d".into(),
            kind: EntryKind::Dir
        }]
    );
}

#[test]
fn a_rename_into_a_folder_made_where_a_file_moved_away_is_reported_with_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    for folder in ["x", "e", "z"] {
        fs::create_dir(root.join(folder)).unwrap();
    }
    fs::write(root.join("x/f"), "f").unwrap();
    fs::write(root.join("z/y.txt"), "y").unwrap();
    let mut watcher = quiet_watcher(&root, &["x", "z"]);
    fs::rename(root.join("x/f"), root.join("e/moved")).unwrap();
    fs::create_dir(root.join("x/f")).unwrap();
    fs::rename(root.join("z/y.txt"), root.join("x/f/y.txt")).unwrap();
    let poll = watcher.poll(Duration::ZERO).unwrap();
    assert!(
        poll.changes.contains(&Change::Renamed {
            from: "z/y.txt".into(),
            to: "x/f/y.txt".into(),
            kind: EntryKind::File
        }),
        "{:?}",
        poll.changes
    );
    assert!(
        (watcher.snapshot().entries).contains_key(Path::new("x/f/y.txt")),
        "{:?}",
        poll.changes
    );
}

#[test]
fn a_file_moved_back_into_an_unreadable_folder_during_a_scan_keeps_its_place_there() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    for folder in ["d", "e", "z"] {
        fs::create_dir(root.join(folder)).unwrap();
    }
    fs::write(root.join("d/f"), "f").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(root.join("d/f"), root.join("e/f")).unwrap();
    fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o300)).unwrap();
    let blocked = blocks_reads(
        "a_file_moved_back_into_an_unreadable_folder_during_a_scan_keeps_its_place_there",
        fs::read_dir(root.join("d")),
    );
    let during = root.clone();
    let rescan = blocked.then(|| {
        snapshot.rescan_with(&root, [PathBuf::new()], None, &mut |cut| {
            if let Cut::OpeningDir(path) = cut
                && path == Path::new("z")
                && during.join("e/f").exists()
            {
                fs::rename(during.join("e/f"), during.join("d/f")).unwrap();
            }
        })
    });
    fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
    let Some(rescan) = rescan else {
        return;
    };
    let changes = rescan.unwrap().changes;
    assert!(
        !changes
            .iter()
            .any(|change| matches!(change, Change::Renamed { .. })),
        "{changes:?}"
    );
    assert!(
        snapshot.entries.contains_key(Path::new("d/f")),
        "{changes:?}"
    );
}

#[test]
fn a_folder_moved_out_of_an_unreadable_folder_takes_along_a_child_the_rules_leave_out() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("x/d/sub")).unwrap();
    fs::create_dir(root.join("e")).unwrap();
    fs::write(root.join("x/d/sub/f"), "f").unwrap();
    let mut rules = Rules::new(Box::new(Lines::default()));
    let mut snapshot = Snapshot::scan_with_rules(&root, &mut rules).unwrap();
    fs::write(
        root.join("x/d/sub").join(Source::Koiignore.file_name()),
        "f\n",
    )
    .unwrap();
    snapshot
        .rescan_with_rules(&root, [PathBuf::new()], &mut rules)
        .unwrap();
    fs::rename(root.join("x/d/sub"), root.join("e/sub")).unwrap();
    fs::set_permissions(root.join("x/d"), fs::Permissions::from_mode(0o300)).unwrap();
    let blocked = blocks_reads(
        "a_folder_moved_out_of_an_unreadable_folder_takes_along_a_child_the_rules_leave_out",
        fs::read_dir(root.join("x/d")),
    );
    let rescan = blocked.then(|| snapshot.rescan_with_rules(&root, [PathBuf::new()], &mut rules));
    fs::set_permissions(root.join("x/d"), fs::Permissions::from_mode(0o755)).unwrap();
    let Some(rescan) = rescan else {
        return;
    };
    let changes = rescan.unwrap().changes;
    assert!(
        !snapshot.entries.contains_key(Path::new("x/d/sub")),
        "{changes:?}"
    );
    assert_eq!(
        snapshot.left_out.keys().collect::<Vec<_>>(),
        [Path::new("e/sub/f")],
        "{changes:?}"
    );
}

#[test]
fn a_child_that_moves_back_into_an_unreadable_folder_during_a_scan_keeps_its_folder_there() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    for folder in ["x/d/sub", "e", "z"] {
        fs::create_dir_all(root.join(folder)).unwrap();
    }
    fs::write(root.join("x/d/sub/s.txt"), "s").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(root.join("x/d/sub"), root.join("e/sub")).unwrap();
    fs::rename(root.join("e/sub/s.txt"), root.join("e/s.txt")).unwrap();
    fs::set_permissions(root.join("x/d"), fs::Permissions::from_mode(0o300)).unwrap();
    let blocked = blocks_reads(
        "a_child_that_moves_back_into_an_unreadable_folder_during_a_scan_keeps_its_folder_there",
        fs::read_dir(root.join("x/d")),
    );
    let during = root.clone();
    let rescan = blocked.then(|| {
        snapshot.rescan_with(&root, [PathBuf::new()], None, &mut |cut| {
            if let Cut::OpeningDir(path) = cut
                && path == Path::new("z")
                && during.join("e/s.txt").exists()
            {
                fs::rename(during.join("e/s.txt"), during.join("x/d/s.txt")).unwrap();
            }
        })
    });
    fs::set_permissions(root.join("x/d"), fs::Permissions::from_mode(0o755)).unwrap();
    let Some(rescan) = rescan else {
        return;
    };
    let changes = rescan.unwrap().changes;
    assert!(
        !(changes.iter()).any(|change| matches!(change, Change::Renamed { .. }))
            && snapshot.entries.contains_key(Path::new("x/d/sub/s.txt")),
        "{changes:?}"
    );
}

#[test]
fn a_volume_root_born_on_one_side_only_comes_back_only_as_a_fat_or_exfat_volume() {
    let (disk, stick) = (VolumeId::Uuid([7; 16]), VolumeId::Uuid([9; 16]));
    let root = |birth_ns, volume| Entry {
        kind: EntryKind::Dir,
        id: FileId {
            ino: 2,
            birth_ns,
            handle: None,
        },
        size: 0,
        mtime_ns: 0,
        ctime: Ctime::Settled { at: 0, mode: 0 },
        executable: false,
        linked: false,
        mount: Some(Mount::Volume(volume)),
    };
    let born = Some(1_700_000_000_000_000_000);
    assert!(!root(None, disk).is_back(&root(born, disk), Some(disk)));
    assert!(!root(born, disk).is_back(&root(None, disk), Some(disk)));
    assert!(root(born, stick).is_back(&root(None, stick), Some(disk)));
}

fn visits<T>(budget: u64, work: impl FnOnce() -> T) -> (T, u64) {
    VISITED.set(0);
    VISIT_BUDGET.set(budget);
    let done = work();
    VISIT_BUDGET.set(u64::MAX);
    (done, VISITED.get())
}

#[derive(Clone, Copy)]
enum Moved {
    OutOfOneFolder,
    EachOutOfItsOwnFolder,
    WithAHardLinkLeftBehind,
}

fn moved_out_of_removed_folders(moved: Moved, files: usize, budget: u64) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join(Source::Koiignore.file_name()), "ig\n").unwrap();
    fs::create_dir_all(root.join("ig/deep")).unwrap();
    fs::write(root.join("ig/deep/old.txt"), "o").unwrap();
    fs::create_dir(root.join("e")).unwrap();
    fs::create_dir(root.join("keep")).unwrap();
    let from = |i: usize| match moved {
        Moved::EachOutOfItsOwnFolder => root.join(format!("d{i}/f")),
        Moved::OutOfOneFolder | Moved::WithAHardLinkLeftBehind => root.join(format!("d/{i}")),
    };
    for i in 0..files {
        fs::create_dir_all(from(i).parent().unwrap()).unwrap();
        fs::write(from(i), "x").unwrap();
        if let Moved::WithAHardLinkLeftBehind = moved {
            fs::hard_link(from(i), root.join(format!("keep/{i}"))).unwrap();
        }
    }
    let mut rules = Rules::new(Box::new(Lines::default()));
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    for i in 0..files {
        let to = files - 1 - i;
        fs::rename(from(i), root.join(format!("e/{to}"))).unwrap();
        if let Moved::EachOutOfItsOwnFolder = moved {
            fs::remove_dir(from(i).parent().unwrap()).unwrap();
        }
    }
    if let Moved::OutOfOneFolder | Moved::WithAHardLinkLeftBehind = moved {
        fs::remove_dir(root.join("d")).unwrap();
    }

    let (rescan, visited) = visits(budget, || {
        snapshot.rescan_with_rules(root, [PathBuf::new()], &mut rules)
    });
    let renamed = (rescan.unwrap().changes.iter())
        .filter(|change| matches!(change, Change::Renamed { .. }))
        .count();
    assert_eq!(renamed, files);
    assert!(visited > 0);
    visited
}

#[test]
fn many_files_moved_out_of_removed_folders_are_matched_in_linear_work() {
    for moved in [
        Moved::OutOfOneFolder,
        Moved::EachOutOfItsOwnFolder,
        Moved::WithAHardLinkLeftBehind,
    ] {
        let five_hundred = moved_out_of_removed_folders(moved, 500, u64::MAX);
        moved_out_of_removed_folders(moved, 5_000, five_hundred * 11);
    }
}

fn hear_barrier(watcher: &NativeWatcher) {
    if let Some(barrier) = &watcher.barrier {
        let event = notify::Event::new(EventKind::Any).add_path(barrier.file.clone());
        watcher.sender.send(Ok(event)).unwrap();
    }
}

fn heard_in_batches(tree: &[&str], moves: &[(&str, &str)], batches: &[&[&str]]) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    for path in tree.iter().chain(&[".koidrive/"]) {
        let at = root.join(path);
        fs::create_dir_all(if path.ends_with('/') {
            &at
        } else {
            at.parent().unwrap()
        })
        .unwrap();
        if !path.ends_with('/') {
            fs::write(&at, path).unwrap();
        }
    }
    let mut watcher = quiet_watcher(&root, &[]);
    let mut whole = watcher.snapshot.clone();
    for (from, to) in moves {
        fs::rename(root.join(from), root.join(to)).unwrap();
    }
    let mut expected = whole.rescan(&root, [PathBuf::new()]).unwrap().changes;
    let mut changes = Vec::new();
    for batch in batches {
        watcher.dirty.extend(batch.iter().map(PathBuf::from));
        changes.extend(watcher.poll(Duration::ZERO).unwrap().changes);
    }
    for _ in 0..3 {
        hear_barrier(&watcher);
        changes.extend(watcher.poll(Duration::ZERO).unwrap().changes);
    }
    changes.sort_by_key(|change| format!("{change:?}"));
    expected.sort_by_key(|change| format!("{change:?}"));
    assert_eq!(changes, expected, "{batches:?}");
    assert_eq!(watcher.snapshot.entries, whole.entries, "{batches:?}");
    assert!(watcher.poll(Duration::ZERO).unwrap().pending.is_empty());
}

#[test]
fn a_move_heard_in_two_batches_is_a_rename_whatever_the_watcher_hears_between() {
    let file: [&[&str]; 2] = [&["x/d/g.txt"], &["e/moved.txt"]];
    let folder: [&[&str]; 2] = [&["x/d"], &["e/d2"]];
    for (from, to) in [(0, 1), (1, 0)] {
        for between in [&[][..], &["x"], &["e"]] {
            heard_in_batches(
                &["x/d/g.txt", "e/"],
                &[("x/d/g.txt", "e/moved.txt")],
                &[file[from], between, file[to]],
            );
            heard_in_batches(
                &["x/d/g.txt", "x/d/h.txt", "e/"],
                &[("x/d", "e/d2")],
                &[folder[from], between, folder[to]],
            );
        }
    }
}

#[test]
fn a_folder_seen_moved_beside_a_scanned_path_is_a_rename_once_its_events_come() {
    let tree = ["d0/f4", "d0/e/f1", "d1/f2", "d1/f3", "d2/"];
    let moves = [("d0/f4", "n1"), ("d1", "d2/n2"), ("d0/e/f1", "d0/n3")];
    heard_in_batches(&tree, &moves, &[&["n1"], &[], &["d2/n2"], &["d0/n3"]]);
    heard_in_batches(&tree, &moves, &[&["n1"], &["d0/n3"], &["d1", "d2/n2"]]);
}

#[test]
fn a_held_removal_is_reported_once_its_barrier_is_heard_or_times_out() {
    for heard in [true, false] {
        let (_dir, root, _) = one_file();
        fs::create_dir(root.join(".koidrive")).unwrap();
        let mut watcher = quiet_watcher(&root, &["x/g.txt"]);
        fs::remove_file(root.join("x/g.txt")).unwrap();
        for batch in [&[][..], &["x"], &["x/g.txt"]] {
            watcher.dirty.extend(batch.iter().map(PathBuf::from));
            let held = watcher.poll(Duration::ZERO).unwrap();
            assert_eq!(
                (held.changes, held.pending),
                (vec![], vec![PathBuf::from("x/g.txt")])
            );
        }
        match heard {
            true => hear_barrier(&watcher),
            false => watcher.barrier.as_mut().unwrap().written -= BARRIER_TIMEOUT,
        }
        assert_eq!(
            watcher.poll(Duration::ZERO).unwrap().changes,
            [Change::Removed {
                path: "x/g.txt".into(),
                kind: EntryKind::File
            }]
        );
        assert!(watcher.barrier.is_none());
    }
}

#[test]
fn a_removal_held_while_an_earlier_barrier_waits_gets_a_barrier_of_its_own() {
    for heard in [true, false] {
        let (_dir, root, _) = one_file();
        fs::create_dir(root.join(".koidrive")).unwrap();
        fs::write(root.join("x/h.txt"), "h").unwrap();
        let mut watcher = quiet_watcher(&root, &["x/g.txt"]);
        let _old = fs::File::open(root.join("x/g.txt")).unwrap();
        fs::remove_file(root.join("x/g.txt")).unwrap();
        let mut changes = watcher.poll(Duration::ZERO).unwrap().changes;
        fs::write(root.join("x/g.txt"), "new").unwrap();
        fs::remove_file(root.join("x/h.txt")).unwrap();
        let batch = ["x/g.txt", "x/h.txt"].map(PathBuf::from);
        watcher.dirty.extend(batch);
        changes.extend(watcher.poll(Duration::ZERO).unwrap().changes);
        match heard {
            true => hear_barrier(&watcher),
            false => watcher.barrier.as_mut().unwrap().written -= BARRIER_TIMEOUT,
        }
        changes.extend(watcher.poll(Duration::ZERO).unwrap().changes);
        hear_barrier(&watcher);
        changes.extend(watcher.poll(Duration::ZERO).unwrap().changes);
        assert_eq!(
            changes,
            [
                Change::Modified {
                    path: "x/g.txt".into()
                },
                Change::Removed {
                    path: "x/h.txt".into(),
                    kind: EntryKind::File
                }
            ],
            "heard {heard}"
        );
        assert!(watcher.barrier.is_none());
    }
}

#[test]
fn a_removal_held_after_an_overflow_gets_a_barrier_of_its_own() {
    let removed = |path: &str| Change::Removed {
        path: path.into(),
        kind: EntryKind::File,
    };
    let (_dir, root, _) = one_file();
    fs::create_dir(root.join(".koidrive")).unwrap();
    fs::write(root.join("x/h.txt"), "h").unwrap();
    let mut watcher = quiet_watcher(&root, &["x/g.txt"]);
    fs::remove_file(root.join("x/g.txt")).unwrap();
    assert_eq!(watcher.poll(Duration::ZERO).unwrap().changes, []);
    let overflow = notify::Event::new(EventKind::Other).set_flag(Flag::Rescan);
    watcher.sender.send(Ok(overflow)).unwrap();
    let changes = watcher.poll(Duration::ZERO).unwrap().changes;
    assert_eq!(changes, [removed("x/g.txt")]);
    (watcher.sender, watcher.events) = mpsc::channel();
    fs::remove_file(root.join("x/h.txt")).unwrap();
    watcher.dirty.insert("x/h.txt".into());
    assert_eq!(watcher.poll(Duration::ZERO).unwrap().changes, []);
    hear_barrier(&watcher);
    let changes = watcher.poll(Duration::ZERO).unwrap().changes;
    assert_eq!(changes, [removed("x/h.txt")]);
}

#[test]
fn a_barrier_does_not_lift_a_hold_made_after_it_was_written() {
    let (_dir, root, _) = one_file();
    fs::create_dir(root.join(".koidrive")).unwrap();
    fs::create_dir(root.join("y")).unwrap();
    let mut watcher = quiet_watcher(&root, &["x/g.txt"]);
    let _old = fs::File::open(root.join("x/g.txt")).unwrap();
    fs::remove_file(root.join("x/g.txt")).unwrap();
    let mut changes = watcher.poll(Duration::ZERO).unwrap().changes;
    fs::write(root.join("x/g.txt"), "new").unwrap();
    watcher.dirty.insert("x/g.txt".into());
    changes.extend(watcher.poll(Duration::ZERO).unwrap().changes);
    fs::rename(root.join("x/g.txt"), root.join("y/g.txt")).unwrap();
    watcher.dirty.insert("x/g.txt".into());
    changes.extend(watcher.poll(Duration::ZERO).unwrap().changes);
    hear_barrier(&watcher);
    changes.extend(watcher.poll(Duration::ZERO).unwrap().changes);
    watcher.dirty.insert("y/g.txt".into());
    hear_barrier(&watcher);
    changes.extend(watcher.poll(Duration::ZERO).unwrap().changes);
    assert_eq!(
        changes,
        [
            Change::Modified {
                path: "x/g.txt".into()
            },
            Change::Renamed {
                from: "x/g.txt".into(),
                to: "y/g.txt".into(),
                kind: EntryKind::File
            }
        ]
    );
}

#[test]
fn a_native_watcher_hears_its_barrier_before_the_timeout() {
    for rules in [false, true] {
        let (_dir, root, _) = one_file();
        fs::create_dir(root.join(".koidrive")).unwrap();
        fs::write(root.join(Source::Koiignore.file_name()), ".koidrive\n").unwrap();
        let previous = Snapshot::scan(&root).unwrap();
        let mut watcher = match rules {
            true => NativeWatcher::start_with_rules(
                &root,
                previous,
                Rules::new(Box::new(Lines::default())),
            ),
            false => NativeWatcher::start(&root, previous),
        }
        .unwrap();
        while !watcher.poll(Duration::ZERO).unwrap().pending.is_empty() {}
        let started = Instant::now();
        fs::remove_file(root.join("x/g.txt")).unwrap();
        let mut changes = Vec::new();
        while changes.is_empty() && started.elapsed() < BARRIER_TIMEOUT {
            changes = watcher.poll(Duration::from_millis(100)).unwrap().changes;
        }
        assert!(started.elapsed() < BARRIER_TIMEOUT, "rules {rules}");
        assert_eq!(
            changes,
            [Change::Removed {
                path: "x/g.txt".into(),
                kind: EntryKind::File
            }],
            "rules {rules}"
        );
    }
}
