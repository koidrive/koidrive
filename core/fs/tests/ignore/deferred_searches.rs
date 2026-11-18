// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_search_through_a_large_ignored_folder_goes_on_across_rescans() {
    for (gone, to) in [
        ("f.txt", Some("ig/zz/deep/f.txt")),
        ("d", Some("ig/zz/deep/d")),
        ("d/g.txt", None),
    ] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "f.txt", "f");
        write(root, "d/g.txt", "g");
        let mut rules = rules().with_search_budget(100);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let before = &snapshot.entries[Path::new(gone)];
        let (id, kind) = (before.id, before.kind);
        match to {
            Some(to) => fs::rename(root.join(gone), root.join(to)).unwrap(),
            None => fs::remove_file(root.join(gone)).unwrap(),
        }
        let mut paths = whole().to_vec();
        let mut rescans = 0;
        let changes = loop {
            rescans += 1;
            assert!(rescans < 20, "{gone}: the search never ended");
            let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            if !rescan.changes.is_empty() {
                break rescan.changes;
            }
            assert_eq!(rescan.unsettled, [PathBuf::from(gone)], "{gone}");
            assert!(!snapshot.entries.contains_key(Path::new(gone)), "{gone}");
            paths = rescan.unsettled;
        };
        assert!(rescans > 2, "{gone}: {rescans} rescans");
        let expected = match to {
            Some(to) => Change::Ignored {
                from: gone.into(),
                to: to.into(),
                kind,
                id,
            },
            None => removed(gone, kind),
        };
        assert_eq!(changes, [expected], "{gone}");
        assert!(!snapshot.entries.contains_key(Path::new(gone)), "{gone}");
    }
}

#[test]
fn a_file_removed_from_a_folder_renamed_while_the_search_runs_waits_at_its_new_path() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/x.txt", "x");
    write(root, "d/y.txt", "y");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::remove_file(root.join("d/x.txt")).unwrap();
    fs::rename(root.join("d"), root.join("e")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Renamed {
            from: "d".into(),
            to: "e".into(),
            kind: Dir,
        }]
    );
    assert_eq!(rescan.unsettled, [PathBuf::from("e/x.txt")]);
    let outside = |snapshot: &Snapshot| -> Vec<String> {
        (known(snapshot).into_iter())
            .filter(|path| path.starts_with('d') || path.starts_with('e'))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(outside(&snapshot), ["e", "e/y.txt"]);

    let mut paths = rescan.unsettled;
    let changes = loop {
        let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
        if !rescan.changes.is_empty() {
            break rescan.changes;
        }
        assert_eq!(rescan.unsettled, [PathBuf::from("e/x.txt")]);
        paths = rescan.unsettled;
    };
    assert_eq!(changes, [removed("e/x.txt", File)]);
    assert_eq!(outside(&snapshot), ["e", "e/y.txt"]);
}

#[test]
fn a_search_budget_of_zero_still_settles() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "f.txt", "f");
    let mut rules = rules().with_search_budget(0);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::remove_file(root.join("f.txt")).unwrap();
    let mut changes = Vec::new();
    for _ in 0..1000 {
        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        changes.extend(rescan.changes);
        if rescan.unsettled.is_empty() {
            break;
        }
    }
    assert_eq!(changes, [removed("f.txt", File)]);
}

#[test]
fn a_file_moved_over_another_out_of_a_removed_folder_is_removed_through_a_bounded_search() {
    for budget in [usize::MAX, 100] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f", "f");
        write(root, "x", "x");
        fs::hard_link(root.join("d/f"), root.join("ig/zz/deep/l")).unwrap();
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        fs::rename(root.join("d/f"), root.join("x")).unwrap();
        fs::remove_dir(root.join("d")).unwrap();

        let mut paths = whole().to_vec();
        let mut changes = Vec::new();
        for _ in 0..50 {
            let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            changes.extend(rescan.changes);
            if rescan.unsettled.is_empty() {
                break;
            }
            paths = rescan.unsettled;
        }
        changes.sort_by_key(|change| format!("{change:?}"));
        assert_eq!(
            changes,
            [Change::Modified { path: "x".into() }, removed("d", Dir)],
            "{budget}"
        );
    }
}

#[test]
fn a_file_moved_over_another_out_of_a_removed_folder_is_removed_by_fresh_rules_through_a_bounded_search()
 {
    for budget in [100, 3] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f", "f");
        write(root, "x", "x");
        fs::hard_link(root.join("d/f"), root.join("ig/zz/deep/l")).unwrap();
        let mut snapshot =
            Snapshot::scan_with_rules(root, &mut rules().with_search_budget(budget)).unwrap();
        fs::rename(root.join("d/f"), root.join("x")).unwrap();
        fs::remove_dir(root.join("d")).unwrap();

        let mut rules = rules().with_search_budget(budget);
        let mut paths = whole().to_vec();
        let mut changes = Vec::new();
        for _ in 0..1000 {
            let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            changes.extend(rescan.changes);
            if rescan.unsettled.is_empty() {
                break;
            }
            paths = rescan.unsettled;
        }
        changes.sort_by_key(|change| format!("{change:?}"));
        assert_eq!(
            changes,
            [Change::Modified { path: "x".into() }, removed("d", Dir)],
            "{budget}"
        );
    }
}

#[test]
fn a_reset_over_a_held_path_then_a_new_file_there_keeps_the_move() {
    for native in [false, true] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f.txt", "old");
        let mut rules = rules().with_search_budget(100);
        let start = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = start.entries[Path::new("d/f.txt")].id;
        let known: BTreeMap<PathBuf, Known> = (start.entries.iter())
            .filter(|(path, _)| path.starts_with("d"))
            .map(|(path, entry)| {
                let known = Known {
                    kind: entry.kind,
                    id: entry.id,
                };
                (path.clone(), known)
            })
            .collect();
        let mut watcher: Box<dyn Watcher> = match native {
            true => Box::new(NativeWatcher::start_with_rules(root, start, rules).unwrap()),
            false => {
                Box::new(RescanWatcher::with_rules(root, start, Duration::ZERO, rules).unwrap())
            }
        };
        fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
        let mut pending = Vec::new();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(50));
            let poll = watcher.poll(Duration::ZERO).unwrap();
            assert_eq!(poll.changes, [], "native {native}");
            pending = poll.pending;
            if !pending.is_empty() {
                break;
            }
        }
        assert_eq!(pending, [PathBuf::from("d/f.txt")], "native {native}");
        watcher.reset(Path::new("d"), known);
        write(root, "d/f.txt", "new");

        let mut changes = Vec::new();
        for _ in 0..50 {
            let poll = watcher.poll(Duration::from_millis(200)).unwrap();
            changes.extend(poll.changes);
            if poll.pending.is_empty() && changes.len() >= 2 {
                break;
            }
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
            ],
            "native {native}"
        );
    }
}

#[test]
fn a_folder_replaced_while_held_keeps_the_file_returned_to_it() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/x", "x");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::rename(root.join("d"), root.join("ig/zz/deep/d")).unwrap();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unsettled, [PathBuf::from("d")]);
    fs::create_dir(root.join("d")).unwrap();
    fs::rename(root.join("ig/zz/deep/d/x"), root.join("d/x")).unwrap();
    fs::remove_dir(root.join("ig/zz/deep/d")).unwrap();

    let mut changes = Vec::new();
    for _ in 0..10 {
        let rescan = snapshot.rescan_with_rules(root, whole(), &mut rules);
        changes.extend(rescan.unwrap().changes);
    }
    assert_eq!(
        changes,
        [
            removed("d", Dir),
            Change::Renamed {
                from: "d/x".into(),
                to: "d/x".into(),
                kind: File,
            },
            created("d", Dir),
        ]
    );
}

#[test]
fn a_held_path_in_a_folder_the_scan_cannot_read_stays_unscanned() {
    use std::os::unix::fs::PermissionsExt;
    for reset in [true, false] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f.txt", "old");
        let mut held = rules().with_search_budget(100);
        let start = Snapshot::scan_with_rules(root, &mut held).unwrap();
        let id = start.entries[Path::new("d/f.txt")].id;
        let known: BTreeMap<PathBuf, Known> = (start.entries.iter())
            .filter(|(path, _)| path.starts_with("d"))
            .map(|(path, entry)| {
                let known = Known {
                    kind: entry.kind,
                    id: entry.id,
                };
                (path.clone(), known)
            })
            .collect();
        let mut a = RescanWatcher::with_rules(root, start, Duration::ZERO, held).unwrap();
        fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
        let poll = a.poll(Duration::ZERO).unwrap();
        assert_eq!(poll.pending, [PathBuf::from("d/f.txt")]);
        let mut watcher = match reset {
            true => {
                a.reset(Path::new("d"), known);
                a
            }
            false => {
                let fresh = rules().with_search_budget(100);
                RescanWatcher::with_rules(root, a.snapshot().clone(), Duration::ZERO, fresh)
                    .unwrap()
            }
        };
        write(root, "d/f.txt", "new");
        fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o000)).unwrap();
        let mut changes = Vec::new();
        for _ in 0..3 {
            changes.extend(watcher.poll(Duration::ZERO).unwrap().changes);
        }
        fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
        for _ in 0..10 {
            let poll = watcher.poll(Duration::ZERO).unwrap();
            changes.extend(poll.changes);
            if poll.pending.is_empty() && changes.len() >= 2 {
                break;
            }
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
            ],
            "reset {reset}"
        );
    }
}

#[test]
fn a_folder_removed_while_the_search_for_its_file_runs_is_removed() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/f.txt", "f");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::remove_file(root.join("d/f.txt")).unwrap();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unsettled, [PathBuf::from("d/f.txt")]);
    fs::remove_dir(root.join("d")).unwrap();

    let mut paths = whole().to_vec();
    for _ in 0..20 {
        let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
        if rescan.unsettled.is_empty() {
            assert_eq!(rescan.changes, [removed("d", Dir)]);
            return;
        }
        assert_eq!(rescan.changes, []);
        paths = rescan.unsettled;
    }
    panic!("d is still unsettled");
}

#[test]
fn a_new_entry_at_the_path_of_a_deferred_move_into_an_ignored_folder_keeps_the_move() {
    for (deferred, renamed) in [(true, false), (true, true), (false, true)] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f.txt", "old");
        let mut rules = rules().with_search_budget(100);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = snapshot.entries[Path::new("d/f.txt")].id;
        fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
        if deferred {
            let rescan = snapshot
                .rescan_with_rules(root, whole(), &mut rules)
                .unwrap();
            assert_eq!(rescan.unsettled, [PathBuf::from("d/f.txt")]);
        }
        let at = if renamed {
            fs::rename(root.join("d"), root.join("e")).unwrap();
            "e/f.txt"
        } else {
            "d/f.txt"
        };
        write(root, at, "new");

        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        assert!(
            !(rescan.changes.iter()).any(|change| matches!(change, Change::Created { .. })),
            "{deferred} {renamed}: {:?}",
            rescan.changes
        );
        let mut changes = rescan.changes;
        let mut unsettled = rescan.unsettled;
        while !unsettled.is_empty() {
            let rescan = snapshot
                .rescan_with_rules(root, whole(), &mut rules)
                .unwrap();
            changes.extend(rescan.changes);
            unsettled = rescan.unsettled;
        }
        let mut expected = Vec::new();
        if renamed {
            expected.push(Change::Renamed {
                from: "d".into(),
                to: "e".into(),
                kind: Dir,
            });
        }
        expected.push(Change::Ignored {
            from: at.into(),
            to: "ig/zz/deep/f.txt".into(),
            kind: File,
            id,
        });
        expected.push(created(at, File));
        assert_eq!(changes, expected, "{deferred} {renamed}");
    }
}

#[test]
fn a_file_moved_over_another_out_of_a_deferred_folder_leaves_with_the_folder() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "b/c/2.txt", "2");
    write(root, "top.txt", "top");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    let id = snapshot.entries[Path::new("b/c")].id;
    let top = snapshot.entries[Path::new("top.txt")].id;
    fs::rename(root.join("top.txt"), root.join("ig/zz/deep/top.txt")).unwrap();
    fs::rename(root.join("b/c/2.txt"), root.join("top.txt")).unwrap();
    fs::rename(root.join("b/c"), root.join("ig/zz/deep/c")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, []);
    assert_eq!(
        rescan.unsettled,
        [PathBuf::from("b/c"), PathBuf::from("top.txt")]
    );
    assert_eq!(known(&snapshot), [RULES_FILE, "b", "top.txt"]);

    let mut paths = rescan.unsettled;
    let changes = loop {
        let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
        if !rescan.changes.is_empty() {
            assert_eq!(rescan.unsettled, [] as [PathBuf; 0]);
            break rescan.changes;
        }
        paths = rescan.unsettled;
    };
    let folder = Change::Ignored {
        from: "b/c".into(),
        to: "ig/zz/deep/c".into(),
        kind: Dir,
        id,
    };
    let expected = if cfg!(target_os = "macos") {
        vec![
            folder,
            created("top.txt", File),
            Change::Ignored {
                from: "top.txt".into(),
                to: "ig/zz/deep/top.txt".into(),
                kind: File,
                id: top,
            },
        ]
    } else {
        vec![
            folder,
            Change::Modified {
                path: "top.txt".into(),
            },
        ]
    };
    assert_eq!(changes, expected);
}

#[test]
fn a_file_moved_over_another_out_of_a_deferred_folder_stays_unsettled_until_rescanned() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "b/c/2.txt", "2");
    write(root, "top.txt", "top");
    let mut bounded = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut bounded).unwrap();
    fs::rename(root.join("top.txt"), root.join("ig/zz/deep/top.txt")).unwrap();
    fs::rename(root.join("b/c/2.txt"), root.join("top.txt")).unwrap();
    fs::rename(root.join("b/c"), root.join("ig/zz/deep/c")).unwrap();

    let mut rescan = snapshot
        .rescan_with_rules(root, whole(), &mut bounded)
        .unwrap();
    while rescan.changes.is_empty() {
        assert!(rescan.unsettled.contains(&PathBuf::from("top.txt")));
        rescan = (snapshot)
            .rescan_with_rules(root, [PathBuf::from("b/c")], &mut bounded)
            .unwrap();
    }
    assert_eq!(rescan.unsettled, [PathBuf::from("top.txt")]);

    let rescan = (snapshot)
        .rescan_with_rules(root, rescan.unsettled, &mut bounded)
        .unwrap();
    assert_ne!(rescan.changes, []);
    assert_eq!(rescan.unsettled, [] as [PathBuf; 0]);
    let disk = Snapshot::scan_with_rules(root, &mut rules()).unwrap();
    assert_eq!(identities(&snapshot), identities(&disk));
}

#[test]
fn a_bounded_search_ends_where_an_unbounded_one_does() {
    for seed in 1..=200u64 {
        let dir = small_tree();
        let root = dir.path();
        let mut changer = Changer {
            random: Random(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
            ..Changer::default()
        };
        let mut bounded = Run::new(root, 3, format!("bounded {seed}"));
        let mut unbounded = Run::new(root, usize::MAX, format!("unbounded {seed}"));
        let gone = |run: &mut Run| {
            let mut gone = std::mem::take(&mut run.synced.gone);
            gone.sort();
            gone
        };
        for step in 1..=10 {
            changer.out.clear();
            for _ in 0..=changer.random.below(3) {
                changer.change(root);
            }
            unbounded.synced.step = step;
            bounded.synced.step = step;
            let unsettled = unbounded.rescan(root, whole().to_vec());
            assert_eq!(unsettled, [] as [PathBuf; 0], "seed {seed}");
            if seed % 2 == 0 {
                bounded.settle(root);
                assert_eq!(bounded.synced.nodes, unbounded.synced.nodes, "seed {seed}");
                assert_eq!(gone(&mut bounded), gone(&mut unbounded), "seed {seed}");
            } else {
                bounded.synced.racy = changer.bounced.clone();
                bounded.rescan(root, whole().to_vec());
            }
        }
        bounded.settle(root);
        let ids = |run: &Run| -> BTreeMap<PathBuf, FileId> {
            (run.synced.nodes.iter())
                .map(|(path, node)| (path.clone(), node.id))
                .collect()
        };
        assert_eq!(ids(&bounded), ids(&unbounded), "seed {seed}");
        assert_eq!(ids(&bounded), identities(&bounded.snapshot), "seed {seed}");
    }
}

/// The driver resets a snapshot to what L holds (design §2.8), and L keeps the
/// entries of excluded nodes.
#[test]
fn a_reset_to_entries_the_rules_leave_out_reports_nothing_for_them() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    let moved = snapshot.entries[Path::new("f.txt")].id;
    fs::rename(root.join("f.txt"), root.join("ig/f.txt")).unwrap();
    let mut held: BTreeMap<PathBuf, Known> = (Snapshot::scan(root).unwrap().entries.iter())
        .map(|(path, entry)| {
            (
                path.clone(),
                Known {
                    kind: entry.kind,
                    id: entry.id,
                },
            )
        })
        .collect();
    assert!(held.contains_key(Path::new("ig/deep")));
    held.remove(Path::new("ig/f.txt"));
    held.insert(
        "f.txt".into(),
        Known {
            kind: File,
            id: moved,
        },
    );

    snapshot.reset(Path::new(""), held);
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            Change::Ignored {
                from: "f.txt".into(),
                to: "ig/f.txt".into(),
                kind: File,
                id: moved
            },
            Change::Modified {
                path: RULES_FILE.into()
            },
            Change::Modified {
                path: "d/g.txt".into()
            },
        ]
    );
    assert_eq!(known(&snapshot), [RULES_FILE, "d", "d/g.txt"]);
}

#[test]
fn a_reset_after_a_move_into_an_ignored_folder_finds_it_again() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/f.txt", "old");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    let d = snapshot.entries[Path::new("d")];
    let x = snapshot.entries[Path::new("d/f.txt")];
    fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
    let first = settle(&mut snapshot, root, &mut rules);
    assert!(
        matches!(first.as_slice(), [Change::Ignored { .. }]),
        "{first:?}"
    );
    write(root, "d/f.txt", "new");
    let mut watcher = RescanWatcher::with_rules(root, snapshot, Duration::ZERO, rules).unwrap();
    let known: BTreeMap<PathBuf, Known> = [
        (
            PathBuf::from("d"),
            Known {
                kind: Dir,
                id: d.id,
            },
        ),
        (
            PathBuf::from("d/f.txt"),
            Known {
                kind: File,
                id: x.id,
            },
        ),
    ]
    .into();
    watcher.reset(Path::new("d"), known);
    let mut changes = Vec::new();
    loop {
        let poll = watcher.poll(Duration::ZERO).unwrap();
        changes.extend(poll.changes);
        if poll.pending.is_empty() {
            break;
        }
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
                id: x.id,
            },
        ]
    );
}

#[test]
fn a_held_removal_whose_path_the_rules_then_leave_out_is_decided() {
    for rules_after in ["ig/\nd/\n", "ig/\n*.txt\n"] {
        for (moved, restarted) in [(true, true), (true, false), (false, true), (false, false)] {
            let case = format!("{rules_after:?} moved {moved} restarted {restarted}");
            let dir = large_ignored_tree();
            let root = dir.path();
            write(root, "d/f.txt", "old");
            let mut rules = rules().with_search_budget(100);
            let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
            let x = snapshot.entries[Path::new("d/f.txt")];
            match moved {
                true => fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap(),
                false => fs::remove_file(root.join("d/f.txt")).unwrap(),
            }
            if restarted {
                write(root, "d/f.txt", "new");
                rules = self::rules().with_search_budget(100);
            }
            let first = snapshot
                .rescan_with_rules(root, whole(), &mut rules)
                .unwrap();
            assert_eq!(first.changes, [], "{case}");
            assert_eq!(first.unsettled, [PathBuf::from("d/f.txt")], "{case}");
            if !restarted {
                write(root, "d/f.txt", "new");
            }
            write(root, RULES_FILE, rules_after);
            let decided = match moved {
                true => Change::Ignored {
                    from: "d/f.txt".into(),
                    to: "ig/zz/deep/f.txt".into(),
                    kind: File,
                    id: x.id,
                },
                false => removed("d/f.txt", File),
            };
            assert_eq!(
                settle(&mut snapshot, root, &mut rules),
                [
                    Change::Modified {
                        path: RULES_FILE.into()
                    },
                    decided
                ],
                "{case}"
            );
        }
    }
}

#[test]
fn an_entry_a_rule_left_out_is_removed_once_it_leaves_the_disk() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "*.tmp\ncache/\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::remove_dir_all(root.join("sub/cache")).unwrap();
    fs::remove_file(root.join("c.tmp")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [removed("c.tmp", File), removed("sub/cache", Dir)]
    );
    assert_eq!(
        snapshot.left_out.keys().collect::<Vec<_>>(),
        [Path::new("sub/a.tmp")]
    );
}

#[test]
fn an_entry_a_rule_left_out_moved_to_an_included_name_is_renamed() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "cache/\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("sub/cache"), root.join("sub/kept")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            Change::Renamed {
                from: "sub/cache".into(),
                to: "sub/kept".into(),
                kind: Dir,
            },
            created("sub/kept/x", Dir),
            created("sub/kept/x/y.txt", File),
        ]
    );
    assert!(snapshot.left_out.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn an_entry_a_rule_left_out_before_its_handle_was_read_moved_to_an_included_name_is_renamed() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "cache/\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let left_out = snapshot.left_out.get_mut(Path::new("sub/cache")).unwrap();
    if left_out.id.handle.take().is_none() {
        return;
    }
    fs::rename(root.join("sub/cache"), root.join("sub/kept")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes[0],
        Change::Renamed {
            from: "sub/cache".into(),
            to: "sub/kept".into(),
            kind: Dir,
        }
    );
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn a_reset_to_an_entry_the_rules_leave_out_that_is_gone_reports_it_removed() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    let held: BTreeMap<PathBuf, Known> = (Snapshot::scan(root).unwrap().entries.iter())
        .map(|(path, entry)| {
            (
                path.clone(),
                Known {
                    kind: entry.kind,
                    id: entry.id,
                },
            )
        })
        .collect();
    fs::remove_dir_all(root.join("ig")).unwrap();

    snapshot.reset(Path::new(""), held);
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let gone: Vec<&Change> = (rescan.changes.iter())
        .filter(|change| matches!(change, Change::Removed { .. }))
        .collect();
    assert_eq!(gone, [&removed("ig", Dir)]);
    assert_eq!(known(&snapshot), [RULES_FILE, "d", "d/g.txt", "f.txt"]);
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn a_rescan_of_a_sibling_reports_an_entry_a_rule_left_out_as_removed_once_it_is_gone() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "cache/\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::remove_dir_all(root.join("sub/cache")).unwrap();
    write(root, "sub/b.txt", "edited");

    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from("sub/b.txt")], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            removed("sub/cache", Dir),
            Change::Modified {
                path: "sub/b.txt".into()
            },
        ]
    );
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn an_entry_a_rule_left_out_moved_to_another_excluded_name_is_removed_once_it_is_gone() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "cache/\nother/\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("sub/cache"), root.join("sub/other")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::remove_dir_all(root.join("sub/other")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [removed("sub/other", Dir)]);
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn an_entry_a_rule_left_out_in_a_renamed_folder_is_removed_once_it_is_gone() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "cache/\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("sub"), root.join("moved")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::remove_dir_all(root.join("moved/cache")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [removed("moved/cache", Dir)]);
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn an_entry_renamed_to_a_name_the_rules_exclude_is_removed_once_it_is_gone() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "cache/\n*.log\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("sub/b.txt"), root.join("sub/b.log")).unwrap();
    let moved = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(
        matches!(&moved.changes[..], [Change::Ignored { from, to, .. }] if from == Path::new("sub/b.txt") && to == Path::new("sub/b.log")),
        "{moved:?}"
    );
    fs::remove_file(root.join("sub/b.log")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [removed("sub/b.log", File)]);
    assert_eq!(
        snapshot.left_out.keys().collect::<Vec<_>>(),
        [Path::new("sub/cache")]
    );
}

#[test]
fn an_entry_a_rule_left_out_in_an_unreadable_folder_is_created_once_included_and_readable() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "b.txt\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o000)).unwrap();
    write(root, RULES_FILE, "c.txt\n");
    let blocked = snapshot.rescan_with_rules(root, whole(), &mut rules);
    fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(blocked.unwrap().unreadable, [PathBuf::from("sub")]);
    assert!(!snapshot.entries.contains_key(Path::new("sub/b.txt")));
    assert!(snapshot.left_out.contains_key(Path::new("sub/b.txt")));

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(
        rescan.changes.contains(&created("sub/b.txt", File)),
        "{rescan:?}"
    );
    assert!(snapshot.entries.contains_key(Path::new("sub/b.txt")));
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn an_entry_a_rule_left_out_in_a_renamed_unreadable_folder_is_created_once_included() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "b.txt\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("sub"), root.join("moved")).unwrap();
    fs::set_permissions(root.join("moved"), fs::Permissions::from_mode(0o000)).unwrap();
    write(root, RULES_FILE, "c.txt\n");
    let blocked = snapshot.rescan_with_rules(root, whole(), &mut rules);
    fs::set_permissions(root.join("moved"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(blocked.unwrap().unreadable, [PathBuf::from("moved")]);
    assert!(!snapshot.entries.contains_key(Path::new("moved/b.txt")));
    assert!(snapshot.left_out.contains_key(Path::new("moved/b.txt")));
    assert!(!snapshot.left_out.contains_key(Path::new("sub/b.txt")));
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(
        rescan.changes.contains(&created("moved/b.txt", File)),
        "{rescan:?}"
    );
}
