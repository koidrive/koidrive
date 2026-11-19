// SPDX-License-Identifier: Apache-2.0

#[test]
fn rescan_watcher_goes_on_with_a_search_without_waiting_for_its_interval() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "f.txt", "f");
    let mut rules = rules().with_search_budget(100);
    let snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    let mut watcher =
        RescanWatcher::with_rules(root, snapshot, Duration::from_secs(3600), rules).unwrap();
    fs::remove_file(root.join("f.txt")).unwrap();

    let poll = watcher.poll(Duration::from_secs(5)).unwrap();
    assert_eq!(poll.changes, [removed("f.txt", File)]);
}

#[test]
fn a_watcher_started_from_the_snapshot_of_another_reports_the_removal_it_held() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "f.txt", "f");
    let mut held = rules().with_search_budget(100);
    let snapshot = Snapshot::scan_with_rules(root, &mut held).unwrap();
    let hour = Duration::from_secs(3600);
    let mut first = RescanWatcher::with_rules(root, snapshot, hour, held).unwrap();
    fs::remove_file(root.join("f.txt")).unwrap();
    let poll = first.poll(Duration::ZERO).unwrap();
    assert_eq!(poll.changes, []);
    assert_eq!(poll.pending, [PathBuf::from("f.txt")]);

    let fresh = rules().with_search_budget(100);
    let mut second =
        RescanWatcher::with_rules(root, first.snapshot().clone(), hour, fresh).unwrap();
    let mut changes = Vec::new();
    for _ in 0..5 {
        changes.extend(second.poll(Duration::from_secs(2)).unwrap().changes);
    }
    assert_eq!(changes, [removed("f.txt", File)]);
}

#[test]
fn a_new_file_at_a_held_path_after_a_watcher_handover_keeps_the_move() {
    for (native, before) in [(false, true), (false, false), (true, true), (true, false)] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f.txt", "old");
        let mut held = rules().with_search_budget(100);
        let start = Snapshot::scan_with_rules(root, &mut held).unwrap();
        let id = start.entries[Path::new("d/f.txt")].id;
        let mut a = RescanWatcher::with_rules(root, start, Duration::ZERO, held).unwrap();
        fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
        let poll = a.poll(Duration::ZERO).unwrap();
        assert_eq!(poll.pending, [PathBuf::from("d/f.txt")]);
        if before {
            write(root, "d/f.txt", "new");
        }
        let seed = a.snapshot().clone();
        let fresh = rules().with_search_budget(100);
        let mut b: Box<dyn Watcher> = match native {
            true => Box::new(NativeWatcher::start_with_rules(root, seed, fresh).unwrap()),
            false => {
                Box::new(RescanWatcher::with_rules(root, seed, Duration::ZERO, fresh).unwrap())
            }
        };
        if !before {
            write(root, "d/f.txt", "new");
        }
        let mut changes = Vec::new();
        for _ in 0..50 {
            let poll = b.poll(Duration::from_millis(200)).unwrap();
            changes.extend(poll.changes);
            if poll.pending.is_empty() && changes.len() >= 2 {
                break;
            }
        }
        let expected = [
            created("d/f.txt", File),
            Change::Ignored {
                from: "d/f.txt".into(),
                to: "ig/zz/deep/f.txt".into(),
                kind: File,
                id,
            },
        ];
        changes.sort_by_key(|change| matches!(change, Change::Ignored { .. }));
        assert_eq!(
            changes, expected,
            "native {native}, new file before {before}"
        );
    }
}

#[test]
fn an_unrelated_first_rescan_keeps_the_search_for_a_path_another_watcher_held() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/f.txt", "old");
    write(root, "q/r/e.txt", "e");
    let mut held = rules().with_search_budget(100);
    let start = Snapshot::scan_with_rules(root, &mut held).unwrap();
    let id = start.entries[Path::new("d/f.txt")].id;
    let mut a = RescanWatcher::with_rules(root, start, Duration::ZERO, held).unwrap();
    fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
    let poll = a.poll(Duration::ZERO).unwrap();
    assert_eq!(poll.pending, [PathBuf::from("d/f.txt")]);
    write(root, "d/f.txt", "new");

    let mut snapshot = a.snapshot().clone();
    let mut fresh = rules().with_search_budget(100);
    let mut changes =
        (snapshot.rescan_with_rules(root, vec![PathBuf::from("q/r/e.txt")], &mut fresh))
            .unwrap()
            .changes;
    for _ in 0..10 {
        let rescan = snapshot.rescan_with_rules(root, whole(), &mut fresh);
        changes.extend(rescan.unwrap().changes);
    }
    changes.sort_by_key(|change| matches!(change, Change::Ignored { .. }));
    assert_eq!(
        changes,
        [
            created("d/f.txt", File),
            Change::Ignored {
                from: "d/f.txt".into(),
                to: "ig/zz/deep/f.txt".into(),
                kind: File,
                id,
            },
        ]
    );
}

#[test]
fn native_watcher_reports_a_rules_change_and_does_not_rescan_below_an_ignored_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "ig/old.txt", "o");
    write(&root, "keep.txt", "k");
    let mut watcher = NativeWatcher::start_with_rules(&root, Snapshot::default(), rules()).unwrap();
    expect(
        &mut watcher,
        &[
            created("ig", Dir),
            created("ig/old.txt", File),
            created("keep.txt", File),
        ],
        &[],
    );

    write(&root, KOIIGNORE, "ig/\n");
    let polls = expect(
        &mut watcher,
        &[created(KOIIGNORE, File)],
        &[rule("", Some("ig/\n"))],
    );
    let changes: Vec<&Change> = polls.iter().flat_map(|poll| &poll.changes).collect();
    assert_eq!(changes, [&created(KOIIGNORE, File)]);
    assert_eq!(known(watcher.snapshot()), [KOIIGNORE, "keep.txt"]);

    // Events from before the rule may still arrive, so let them pass.
    while watcher
        .poll(Duration::from_millis(300))
        .unwrap()
        .root
        .is_some()
    {}
    write(&root, "ig/new.txt", "n");
    write(&root, "ig/sub/deep.txt", "d");
    for _ in 0..3 {
        let poll = watcher.poll(Duration::from_millis(300)).unwrap();
        assert_eq!(poll, Poll::default(), "an event below `ig` caused a scan");
    }

    write(&root, "later.txt", "l");
    expect(&mut watcher, &[created("later.txt", File)], &[]);
    assert_eq!(
        known(watcher.snapshot()),
        [KOIIGNORE, "keep.txt", "later.txt"]
    );
}

#[test]
fn native_watcher_reports_a_file_moved_into_an_ignored_folder_as_ignored() {
    let (dir, snapshot, _) = tree_with_an_ignored_folder();
    let root = dir.path().canonicalize().unwrap();
    let id = snapshot.entries[Path::new("f.txt")].id;
    let mut watcher = NativeWatcher::start_with_rules(&root, snapshot, rules()).unwrap();
    expect(&mut watcher, &[], &[rule("", Some("ig/\n"))]);

    fs::rename(root.join("f.txt"), root.join("ig/f.txt")).unwrap();
    let polls = expect(
        &mut watcher,
        &[Change::Ignored {
            from: "f.txt".into(),
            to: "ig/f.txt".into(),
            kind: File,
            id,
        }],
        &[],
    );
    assert_eq!(polls.iter().flat_map(|poll| &poll.changes).count(), 1);
}

#[test]
fn rescan_watcher_applies_the_rules_and_reports_them() {
    let (dir, snapshot, _) = tree_with_an_ignored_folder();
    let root = dir.path();
    let id = snapshot.entries[Path::new("f.txt")].id;
    let mut watcher =
        RescanWatcher::with_rules(root, snapshot, Duration::from_millis(20), rules()).unwrap();
    let poll = watcher.poll(Duration::from_secs(5)).unwrap();
    assert_eq!(poll.changes, []);
    assert_eq!(poll.rules, [rule("", Some("ig/\n"))]);

    fs::rename(root.join("f.txt"), root.join("ig/f.txt")).unwrap();
    write(root, "ig/more.txt", "m");
    let poll = watcher.poll(Duration::from_secs(5)).unwrap();
    assert_eq!(
        poll.changes,
        [Change::Ignored {
            from: "f.txt".into(),
            to: "ig/f.txt".into(),
            kind: File,
            id,
        }]
    );
    assert_eq!(poll.rules, []);
    assert_eq!(known(watcher.snapshot()), [KOIIGNORE, "d", "d/g.txt"]);
}

#[cfg(target_os = "linux")]
#[test]
fn no_inotify_watch_is_registered_below_an_ignored_folder() {
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, KOIIGNORE, "ig/\n");
    write(&root, "ig/deep/x", "x");
    write(&root, "keep/sub/y", "y");
    let inode = |path: &str| fs::metadata(root.join(path)).unwrap().ino();
    let watched = |path: &str| watched_inodes().contains(&inode(path));

    let mut watcher = NativeWatcher::start_with_rules(&root, Snapshot::default(), rules()).unwrap();
    expect(&mut watcher, &[created("keep/sub/y", File)], &[]);
    for path in ["", "keep", "keep/sub"] {
        assert!(watched(path), "{path:?} is not watched");
    }
    for path in ["ig", "ig/deep"] {
        assert!(!watched(path), "{path:?} is watched");
    }

    write(&root, "keep/new/ig/deep/x", "x");
    write(&root, "keep/new/in/z", "z");
    fs::create_dir(root.join("ig/later")).unwrap();
    expect(&mut watcher, &[created("keep/new/in/z", File)], &[]);
    for path in ["keep/new", "keep/new/in"] {
        assert!(watched(path), "{path:?} is not watched");
    }
    for path in ["ig", "ig/later", "keep/new/ig", "keep/new/ig/deep"] {
        assert!(!watched(path), "{path:?} is watched");
    }

    write(&root, "keep/new/in/late", "l");
    expect(&mut watcher, &[created("keep/new/in/late", File)], &[]);
}

/// A folder that a reset makes known is never reported as created, which is
/// how the watcher on Linux learns of a folder to register.
#[test]
fn native_watcher_reports_a_change_below_a_folder_that_a_reset_made_known() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "keep.txt", "k");
    let mut watcher = NativeWatcher::start_with_rules(&root, Snapshot::default(), rules()).unwrap();
    expect(&mut watcher, &[created("keep.txt", File)], &[]);

    fs::create_dir_all(root.join("made/sub")).unwrap();
    let known = (Snapshot::scan(&root).unwrap().entries.into_iter())
        .filter(|(path, _)| path.starts_with("made"))
        .map(|(path, entry)| {
            let (kind, id) = (entry.kind, entry.id);
            (path, Known { kind, id })
        })
        .collect();
    watcher.reset(Path::new("made"), known);
    let poll = watcher.poll(Duration::from_millis(500)).unwrap();
    assert_eq!(
        poll.changes,
        [],
        "the reset told the watcher of the folders"
    );

    write(&root, "made/sub/new.txt", "n");
    expect(&mut watcher, &[created("made/sub/new.txt", File)], &[]);
}

#[test]
fn a_first_rescan_from_a_snapshot_another_watcher_held_does_not_finish_the_search_at_once() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/f.txt", "old");
    let mut held = rules().with_search_budget(100);
    let start = Snapshot::scan_with_rules(root, &mut held).unwrap();
    let id = start.entries[Path::new("d/f.txt")].id;
    let mut a = RescanWatcher::with_rules(root, start, Duration::ZERO, held).unwrap();
    fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
    assert_eq!(
        a.poll(Duration::ZERO).unwrap().pending,
        [PathBuf::from("d/f.txt")]
    );
    write(root, "d/f.txt", "new");

    let mut snapshot = a.snapshot().clone();
    let mut fresh = rules().with_search_budget(100);
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut fresh)
        .unwrap();
    assert_eq!(first.changes, []);
    assert_eq!(first.unsettled, [PathBuf::from("d/f.txt")]);
    let mut changes = Vec::new();
    let mut unsettled = first.unsettled;
    while !unsettled.is_empty() {
        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut fresh)
            .unwrap();
        changes.extend(rescan.changes);
        unsettled = rescan.unsettled;
    }
    changes.sort_by_key(|change| matches!(change, Change::Ignored { .. }));
    assert_eq!(
        changes,
        [
            created("d/f.txt", File),
            Change::Ignored {
                from: "d/f.txt".into(),
                to: "ig/zz/deep/f.txt".into(),
                kind: File,
                id,
            },
        ]
    );
}

#[test]
fn a_save_by_rename_while_no_watcher_ran_is_a_modification() {
    for renamed in [false, true] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f.txt", "old");
        let mut before = rules().with_search_budget(100);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut before).unwrap();
        save(root, "d/f.txt", "new");
        let mut after = rules().with_search_budget(100);
        let first = snapshot
            .rescan_with_rules(root, whole(), &mut after)
            .unwrap();
        assert_eq!(first.changes, [], "{renamed}");
        if renamed {
            fs::rename(root.join("d"), root.join("e")).unwrap();
        }
        let changes = settle(&mut snapshot, root, &mut after);
        let expected = match renamed {
            false => vec![Change::Modified {
                path: "d/f.txt".into(),
            }],
            true => vec![
                Change::Renamed {
                    from: "d".into(),
                    to: "e".into(),
                    kind: Dir,
                },
                Change::Modified {
                    path: "e/f.txt".into(),
                },
            ],
        };
        assert_eq!(changes, expected, "{renamed}");
    }
}

#[test]
fn a_rescan_watcher_taking_over_from_another_reports_what_the_other_held_back() {
    for native_first in [false, true] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "b/c/2.txt", "2");
        write(root, "b/c/3.txt", "3");
        write(root, "p/top.txt", "top");
        let mut held = rules().with_search_budget(100);
        let start = Snapshot::scan_with_rules(root, &mut held).unwrap();
        let hour = Duration::from_secs(3600);
        let mut first: Box<dyn Watcher> = match native_first {
            true => Box::new(NativeWatcher::start_with_rules(root, start, held).unwrap()),
            false => Box::new(RescanWatcher::with_rules(root, start, hour, held).unwrap()),
        };
        let wait = match native_first {
            true => Duration::from_millis(500),
            false => Duration::ZERO,
        };
        assert_eq!(first.poll(wait).unwrap().changes, []);
        fs::rename(root.join("p/top.txt"), root.join("ig/zz/deep/top.txt")).unwrap();
        fs::rename(root.join("b/c/2.txt"), root.join("p/top.txt")).unwrap();
        fs::rename(root.join("b/c"), root.join("ig/zz/deep/c")).unwrap();
        let poll = first.poll(wait).unwrap();
        assert_eq!(poll.changes, [], "native {native_first}");
        assert_eq!(
            poll.pending,
            [PathBuf::from("b/c"), PathBuf::from("p/top.txt")],
            "native {native_first}"
        );
        let c = first.snapshot().entries[Path::new("b/c")].id;
        let top = first.snapshot().entries[Path::new("p/top.txt")].id;

        let fresh = rules().with_search_budget(100);
        let mut second =
            RescanWatcher::with_rules(root, first.snapshot().clone(), Duration::ZERO, fresh)
                .unwrap();
        let mut changes = Vec::new();
        for _ in 0..200 {
            let poll = second.poll(Duration::from_millis(50)).unwrap();
            changes.extend(poll.changes);
            if poll.pending.is_empty() {
                break;
            }
        }
        changes.sort_by_key(|change| format!("{change:?}"));
        let ignored = |from: &str, to: &str, kind, id| Change::Ignored {
            from: from.into(),
            to: to.into(),
            kind,
            id,
        };
        let expected = match cfg!(target_os = "macos") {
            true => vec![
                created("p/top.txt", File),
                ignored("b/c", "ig/zz/deep/c", Dir, c),
                ignored("p/top.txt", "ig/zz/deep/top.txt", File, top),
            ],
            false => vec![
                ignored("b/c", "ig/zz/deep/c", Dir, c),
                Change::Modified {
                    path: "p/top.txt".into(),
                },
            ],
        };
        assert_eq!(changes, expected, "native {native_first}");
        let disk = Snapshot::scan_with_rules(root, &mut rules()).unwrap();
        assert_eq!(
            identities(second.snapshot()),
            identities(&disk),
            "native {native_first}"
        );
    }
}
