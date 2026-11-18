// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_root_replaced_while_a_rescan_widens_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let replacement = dir.path().join("replacement");
    for path in [&root, &replacement] {
        fs::create_dir(path).unwrap();
        fs::write(path.join("a"), "a").unwrap();
        fs::write(path.join("b"), "b").unwrap();
    }
    let mut snapshot = Snapshot::scan(&root).unwrap();
    let before = snapshot.clone();
    for (from, to) in [("a", "tmp"), ("b", "a"), ("tmp", "b")] {
        fs::rename(root.join(from), root.join(to)).unwrap();
    }
    let mut replaced = false;
    let result = snapshot.rescan_with(&root, [PathBuf::from("a")], None, &mut |cut| {
        if matches!(cut, Cut::RootOpened) && !replaced {
            replaced = true;
            fs::rename(&root, dir.path().join("previous")).unwrap();
            fs::rename(&replacement, &root).unwrap();
        }
    });
    assert!(matches!(result, Err(Error::RootReplaced)), "{result:?}");
    assert_eq!(snapshot, before);
}

#[test]
fn a_rotation_seen_from_one_path_takes_at_most_three_scans() {
    const FILES: u64 = 64;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for i in 0..FILES {
        fs::write(root.join(i.to_string()), i.to_string()).unwrap();
    }
    let mut snapshot = Snapshot::scan(root).unwrap();
    let mut whole = snapshot.clone();
    fs::rename(root.join("0"), root.join("tmp")).unwrap();
    for i in 1..FILES {
        fs::rename(root.join(i.to_string()), root.join((i - 1).to_string())).unwrap();
    }
    fs::rename(root.join("tmp"), root.join((FILES - 1).to_string())).unwrap();
    let found = std::cell::Cell::new(0);
    let mut scans = 0;
    let rescan = snapshot
        .rescan_under(
            root,
            [PathBuf::from("0")],
            &BTreeSet::new(),
            None,
            None,
            &|| false,
            &|_| found.set(found.get() + 1),
            Keep::Unplaced,
            &mut |cut| scans += u64::from(matches!(cut, Cut::RootOpened)),
            &MarkLog::default(),
        )
        .unwrap();
    assert_eq!(
        rescan.changes,
        whole.rescan(root, [PathBuf::new()]).unwrap().changes
    );
    assert_eq!(rescan.changes.len() as u64, FILES);
    assert_eq!(snapshot, whole);
    assert!(scans <= 3, "{scans} scans");
    assert!(found.get() <= FILES + 3, "{} files found", found.get());
}

#[test]
fn a_file_moved_over_another_from_a_folder_that_turns_unreadable_replaces_it() {
    moved_from_a_folder_that_turns_unreadable(
        "a_file_moved_over_another_from_a_folder_that_turns_unreadable_replaces_it",
        "doc",
        &[
            Change::Removed {
                path: PathBuf::from("d/f"),
                kind: EntryKind::File,
            },
            Change::Modified {
                path: PathBuf::from("doc"),
            },
        ],
    );
}

#[test]
fn a_file_moved_to_a_new_name_from_a_folder_that_turns_unreadable_is_a_rename() {
    moved_from_a_folder_that_turns_unreadable(
        "a_file_moved_to_a_new_name_from_a_folder_that_turns_unreadable_is_a_rename",
        "moved",
        &[Change::Renamed {
            from: PathBuf::from("d/f"),
            to: PathBuf::from("moved"),
            kind: EntryKind::File,
        }],
    );
}

fn moved_from_a_folder_that_turns_unreadable(test: &str, to: &str, moved: &[Change]) {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let locked = root.join("d");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("f"), "report v2").unwrap();
    fs::write(root.join("doc"), "report v1").unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();
    fs::rename(locked.join("f"), root.join(to)).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let probe = fs::read_dir(&locked).map(|_| ());
    if !blocks_reads(test, probe) {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let first = snapshot.rescan(root, [PathBuf::from(to)]);
    let whole = snapshot.rescan(root, [PathBuf::new()]);
    fs::write(root.join(to), "report v3, edited").unwrap();
    let edited = snapshot.rescan(root, [PathBuf::from(to)]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    let first = first.unwrap();
    assert_eq!(first.changes, moved);
    assert_eq!(first.unsettled, Vec::<PathBuf>::new());
    let whole = whole.unwrap();
    assert_eq!(whole.changes, []);
    assert_eq!(whole.unsettled, Vec::<PathBuf>::new());
    assert_eq!(
        edited.unwrap().changes,
        [Change::Modified {
            path: PathBuf::from(to),
        }]
    );
}

#[test]
fn a_folder_swapped_for_one_it_cannot_list_keeps_both_identities() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in ["x", "y"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("note"), name).unwrap();
    }
    let mut snapshot = Snapshot::scan(root).unwrap();
    let before = snapshot.clone();
    for (from, to) in [("x", "tmp"), ("y", "x"), ("tmp", "y")] {
        fs::rename(root.join(from), root.join(to)).unwrap();
    }
    fs::set_permissions(root.join("x"), fs::Permissions::from_mode(0o000)).unwrap();
    let probe = fs::read_dir(root.join("x")).map(|_| ());
    if !blocks_reads(
        "a_folder_swapped_for_one_it_cannot_list_keeps_both_identities",
        probe,
    ) {
        fs::set_permissions(root.join("x"), fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let rescan = snapshot.rescan(root, [PathBuf::from("y")]);
    fs::set_permissions(root.join("x"), fs::Permissions::from_mode(0o700)).unwrap();
    let rescan = rescan.unwrap();
    assert_eq!(rescan.unreadable, [PathBuf::from("x")]);
    assert_ne!(
        snapshot.entries[Path::new("x")].id,
        snapshot.entries[Path::new("y")].id
    );
    let mut whole = before;
    let expected = whole.rescan(root, [PathBuf::new()]).unwrap().changes;
    let mut actual = rescan.changes;
    actual.extend(snapshot.rescan(root, rescan.unsettled).unwrap().changes);
    assert_eq!(actual, expected);
    assert_eq!(snapshot, whole);
}

fn scoped(files: usize) -> Vec<PathBuf> {
    (0..files)
        .map(|i| PathBuf::from(format!("f{i}.txt")))
        .collect()
}

#[test]
fn root_moved_after_opening_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    tree(&root, 200);
    for paths in [vec![PathBuf::new()], scoped(200)] {
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        let moved = dir.path().join("moved");
        let result = snapshot.rescan_with(&root, paths, None, &mut |cut| {
            if let Cut::RootOpened = cut {
                fs::rename(&root, &moved).unwrap();
            }
        });
        assert!(
            matches!(&result, Err(Error::Io(err)) if err.kind() == io::ErrorKind::NotFound),
            "{result:?}"
        );
        assert_eq!(snapshot, before);
        fs::rename(&moved, &root).unwrap();
    }
}

#[test]
fn root_removed_after_opening_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    for paths in [vec![PathBuf::new()], scoped(200)] {
        tree(&root, 200);
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        let result = snapshot.rescan_with(&root, paths, None, &mut |cut| {
            if let Cut::RootOpened = cut {
                fs::remove_dir_all(&root).unwrap();
            }
        });
        assert!(
            matches!(&result, Err(Error::Io(err)) if err.kind() == io::ErrorKind::NotFound),
            "{result:?}"
        );
        assert_eq!(snapshot, before);
    }
}

#[test]
fn root_replaced_after_opening_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    tree(&root, 20);
    let mut snapshot = Snapshot::scan(&root).unwrap();
    let before = snapshot.clone();
    let result = snapshot.rescan_with(&root, [PathBuf::new()], None, &mut |cut| {
        if let Cut::RootOpened = cut {
            fs::rename(&root, dir.path().join("moved")).unwrap();
            fs::create_dir(&root).unwrap();
        }
    });
    assert!(matches!(result, Err(Error::RootReplaced)), "{result:?}");
    assert_eq!(snapshot, before);
}

#[test]
fn folder_replaced_during_traversal_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("x"), "outside").unwrap();
    for replacement in ["symlink", "folder"] {
        tree(&root, 0);
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        let result = snapshot.rescan_with(&root, [PathBuf::new()], None, &mut |cut| {
            if let Cut::OpeningDir(path) = cut
                && path == Path::new("d")
            {
                fs::rename(root.join("d"), root.join("d-old")).unwrap();
                match replacement {
                    "symlink" => std::os::unix::fs::symlink(&outside, root.join("d")).unwrap(),
                    _ => fs::create_dir(root.join("d")).unwrap(),
                }
            }
        });
        assert!(
            matches!(result, Err(Error::Changed)),
            "{replacement}: {result:?}"
        );
        assert_eq!(snapshot, before, "{replacement}");
        fs::remove_dir_all(&root).unwrap();
    }
}

#[test]
fn ancestor_replaced_after_opening_publishes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("z-victim"), "outside").unwrap();
    let scopes = [
        vec![PathBuf::new()],
        vec![PathBuf::from("d/a-trigger"), PathBuf::from("d/z-victim")],
    ];
    for replacement in ["unreadable", "symlink"] {
        for paths in scopes.clone() {
            fs::create_dir_all(root.join("d/a-trigger")).unwrap();
            fs::write(root.join("d/z-victim"), "user content").unwrap();
            let detached = dir.path().join("detached-d");
            let mut snapshot = Snapshot::scan(&root).unwrap();
            let before = snapshot.clone();

            let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
                if let Cut::OpeningDir(path) = cut
                    && path == Path::new("d/a-trigger")
                {
                    fs::rename(root.join("d"), &detached).unwrap();
                    match replacement {
                        "unreadable" => {
                            fs::create_dir(root.join("d")).unwrap();
                            fs::write(root.join("d/z-victim"), "new user content").unwrap();
                            fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o000))
                                .unwrap();
                        }
                        _ => std::os::unix::fs::symlink(&outside, root.join("d")).unwrap(),
                    }
                    fs::remove_file(detached.join("z-victim")).unwrap();
                }
            });

            assert!(
                matches!(result, Err(Error::Changed)),
                "{replacement} {paths:?}: {result:?}"
            );
            assert_eq!(snapshot, before, "{replacement} {paths:?}");
            if replacement == "unreadable" {
                fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
            }
            fs::remove_dir_all(&root).unwrap();
            fs::remove_dir_all(&detached).unwrap();
        }
    }
}

#[test]
fn ancestor_swapped_out_and_back_publishes_no_removal() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let detached = dir.path().join("detached-d");
    let replacement = dir.path().join("replacement-d");
    let scopes = [
        vec![PathBuf::new()],
        vec![
            PathBuf::from("d/a-trigger"),
            PathBuf::from("d/m-victim"),
            PathBuf::from("d/z-trigger"),
        ],
    ];
    for paths in scopes {
        fs::create_dir_all(root.join("d/a-trigger")).unwrap();
        fs::create_dir_all(root.join("d/z-trigger")).unwrap();
        fs::write(root.join("d/m-victim"), "user content").unwrap();
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();

        let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
            let Cut::OpeningDir(path) = cut else { return };
            if path == Path::new("d/a-trigger") {
                fs::rename(root.join("d"), &detached).unwrap();
                fs::create_dir(root.join("d")).unwrap();
                fs::write(root.join("d/m-victim"), "user content").unwrap();
                fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o000)).unwrap();
                fs::remove_file(detached.join("m-victim")).unwrap();
            } else if path == Path::new("d/z-trigger") {
                fs::write(detached.join("m-victim"), "user content").unwrap();
                fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
                fs::rename(root.join("d"), &replacement).unwrap();
                fs::rename(&detached, root.join("d")).unwrap();
            }
        });

        let rescan = result.unwrap_or_else(|err| panic!("{paths:?}: {err:?}"));
        assert_eq!(rescan.changes, [], "{paths:?}");
        assert_eq!(rescan.unsettled, [PathBuf::from("d/m-victim")], "{paths:?}");
        assert_eq!(snapshot, before, "{paths:?}");
        assert!(root.join("d/m-victim").exists());
        fs::remove_dir_all(&replacement).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}

#[test]
fn parent_swapped_during_confirmation_publishes_no_removal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let detached = dir.path().join("detached-d");
    for paths in [vec![PathBuf::new()], vec![PathBuf::from("d/sub/victim")]] {
        fs::create_dir_all(root.join("d/sub")).unwrap();
        fs::write(root.join("d/sub/victim"), "user content").unwrap();
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        fs::remove_file(root.join("d/sub/victim")).unwrap();

        let mut swapped = false;
        let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
            let at_d = matches!(cut, Cut::Confirming(path) if path == Path::new("d"));
            if swapped || !at_d {
                return;
            }
            swapped = true;
            fs::rename(root.join("d"), &detached).unwrap();
            fs::create_dir_all(root.join("d/sub")).unwrap();
            fs::write(root.join("d/sub/victim"), "user content").unwrap();
        });

        assert!(swapped, "{paths:?}");
        let rescan = result.unwrap_or_else(|err| panic!("{paths:?}: {err:?}"));
        assert_eq!(rescan.changes, [], "{paths:?}");
        assert_eq!(
            rescan.unsettled,
            [PathBuf::from("d/sub/victim")],
            "{paths:?}"
        );
        assert_eq!(snapshot, before, "{paths:?}");
        assert!(root.join("d/sub/victim").exists());
        fs::remove_dir_all(&detached).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}

#[test]
fn sibling_recreated_after_its_parent_was_listed_publishes_no_removal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let files = ["d/a-first", "d/m/y", "d/z-victim"];
    let scopes = [
        vec![PathBuf::new()],
        files.iter().map(PathBuf::from).collect(),
    ];
    for paths in scopes {
        fs::create_dir_all(root.join("d/m")).unwrap();
        for file in files {
            fs::write(root.join(file), "user content").unwrap();
        }
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let mut expected = snapshot.clone();
        expected.entries.remove(Path::new("d/m/y"));
        for file in files {
            fs::remove_file(root.join(file)).unwrap();
        }

        let mut recreated = false;
        let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
            let at_m = matches!(cut, Cut::Confirming(path) if path == Path::new("d/m"));
            if recreated || !at_m {
                return;
            }
            recreated = true;
            fs::write(root.join("d/z-victim"), "recreated by the user").unwrap();
        });

        assert!(recreated, "{paths:?}");
        let rescan = result.unwrap_or_else(|err| panic!("{paths:?}: {err:?}"));
        assert_eq!(
            rescan.changes,
            [Change::Removed {
                path: PathBuf::from("d/m/y"),
                kind: EntryKind::File
            }],
            "{paths:?}"
        );
        assert_eq!(
            rescan.unsettled,
            [PathBuf::from("d/a-first"), PathBuf::from("d/z-victim")],
            "{paths:?}"
        );
        assert_eq!(snapshot, expected, "{paths:?}");

        let settled = snapshot.rescan(&root, rescan.unsettled).unwrap();
        assert_eq!(
            settled,
            Rescan {
                changes: vec![
                    Change::Removed {
                        path: PathBuf::from("d/a-first"),
                        kind: EntryKind::File
                    },
                    Change::Modified {
                        path: PathBuf::from("d/z-victim")
                    },
                ],
                unsettled: vec![],
                unreadable: vec![],
                root: root_id(&root),
                rules: vec![],
                read: vec![],
                settled: vec![],
            },
            "{paths:?}"
        );
        assert_eq!(snapshot, Snapshot::scan(&root).unwrap(), "{paths:?}");
        fs::remove_dir_all(&root).unwrap();
    }
}

#[test]
fn a_change_time_on_a_whole_second_or_on_another_volume_than_its_folder_waits_out_the_coarser_tick()
{
    assert_eq!(tick_of(1_500_000_001, Some(KERNEL_TICK)), KERNEL_TICK);
    assert_eq!(tick_of(2_000_000_000, Some(KERNEL_TICK)), SECOND_TICK);
    assert_eq!(tick_of(2_000_000_000, Some(COARSE_TICK)), COARSE_TICK);
    assert_eq!(tick_of(1_500_000_001, None), COARSE_TICK);
}

#[test]
fn a_swap_seen_halfway_through_a_scan_publishes_nothing_and_the_next_scan_reads_it_as_renames() {
    let swap = [("a.txt", "t.tmp"), ("b.txt", "a.txt"), ("t.tmp", "b.txt")];
    let interleavings = [(&swap[..0], &swap[..]), (&swap[..1], &swap[1..])];
    for ((at_a, at_b), scope) in interleavings
        .into_iter()
        .flat_map(|steps| [(steps, ""), (steps, "a.txt")])
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "a").unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();
        let mut snapshot = Snapshot::scan(root).unwrap();
        let before = snapshot.clone();
        let (mut at_a, mut at_b) = (Some(at_a), Some(at_b));
        let paths = [PathBuf::from(scope), PathBuf::from("b.txt")];
        let result = snapshot.rescan_with(root, paths, None, &mut |cut| {
            let renames = match cut {
                Cut::Stating(path) if path == Path::new("a.txt") => at_a.take(),
                Cut::Stating(path) if path == Path::new("b.txt") => at_b.take(),
                _ => None,
            };
            for (from, to) in renames.into_iter().flatten() {
                fs::rename(root.join(from), root.join(to)).unwrap();
            }
        });
        assert!(
            matches!(result, Err(Error::Changed)),
            "{scope:?}: {result:?}"
        );
        assert_eq!(snapshot, before);
        let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
        assert_eq!(
            changes,
            [
                Change::Renamed {
                    from: "a.txt".into(),
                    to: "b.txt".into(),
                    kind: EntryKind::File
                },
                Change::Renamed {
                    from: "b.txt".into(),
                    to: "a.txt".into(),
                    kind: EntryKind::File
                },
            ],
            "{scope:?}"
        );
    }
}
