// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_failed_scan_leaves_the_rules_as_they_were() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    for (path, body) in [
        (".koiignore", "ig\n"),
        ("a/.koiignore", "long-name\n"),
        ("a/long-name", "x"),
        ("a/y", "y"),
        ("ig/f", "f"),
        ("m/f", "f"),
        ("z/f", "f"),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let matcher = Lines::default();
    let mut rules = Rules::new(Box::new(matcher.clone()));
    let mut snapshot = Snapshot::scan_with_rules(&root, &mut rules).unwrap();
    let before = snapshot.clone();
    let given = matcher.0.lock().unwrap().clone();
    assert_eq!(given.len(), 2);

    fs::write(root.join(".koiignore"), "z\n").unwrap();
    fs::write(root.join("a/.koiignore"), "y\n").unwrap();
    let result = snapshot.rescan_under(
        &root,
        [PathBuf::new()],
        &BTreeSet::new(),
        None,
        Some(&mut rules),
        &|| false,
        &|_| {},
        Keep::Unplaced,
        &mut |cut| {
            if let Cut::OpeningDir(path) = cut
                && path == Path::new("m")
            {
                assert_ne!(*matcher.0.lock().unwrap(), given);
                fs::rename(root.join("m"), root.join("m-old")).unwrap();
                fs::create_dir(root.join("m")).unwrap();
            }
        },
        &MarkLog::default(),
    );
    assert!(matches!(result, Err(Error::Changed)), "{result:?}");
    assert_eq!(snapshot, before);
    assert_eq!(*matcher.0.lock().unwrap(), given);
    assert_eq!(rules.status(Path::new("ig"), EntryKind::Dir), Ignored::Rule);
    assert_eq!(rules.status(Path::new("z"), EntryKind::Dir), Ignored::No);

    fs::remove_dir(root.join("m")).unwrap();
    fs::rename(root.join("m-old"), root.join("m")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(&root, [PathBuf::new()], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.rules,
        [
            (PathBuf::new(), Some(b"z\n".to_vec())),
            (PathBuf::from("a"), Some(b"y\n".to_vec())),
        ]
    );
    assert_eq!(
        rescan.changes,
        [
            created("a/long-name", EntryKind::File),
            created("ig", EntryKind::Dir),
            created("ig/f", EntryKind::File),
            Change::Modified {
                path: ".koiignore".into()
            },
            Change::Modified {
                path: "a/.koiignore".into()
            },
        ]
    );
}

#[test]
fn a_crash_while_trashing_brings_nothing_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("report.txt"), "synced").unwrap();
    let space = Space::bind(fs::File::open(root).unwrap().into(), root).unwrap();
    let location = Location::At {
        base: Base::Root,
        dirs: Vec::new(),
        name: "report.txt".into(),
    };
    let (folder, name) = space.parent(&location).unwrap();
    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = space.trash_with(&folder, name, "e1", name, None, now, &mut || {
            panic!("crash")
        });
    }));
    assert!(crash.is_err());

    let mut recovered = Vec::new();
    for later in [now, now + TRASH_RETENTION, now + 2 * TRASH_RETENTION] {
        recovered.extend(
            space
                .reclaim_trash(later, u64::MAX, &mut |_, _| Ok(true))
                .unwrap()
                .recovered,
        );
    }
    assert_eq!(recovered, Vec::<OsString>::new());
}

#[test]
fn a_search_looks_at_a_bounded_number_of_entries_per_rescan_under_deep_and_wide_left_out_folders() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let deep: PathBuf = ["ig", "wide"].into_iter().chain(["d"; 100]).collect();
    fs::create_dir_all(root.join(&deep)).unwrap();
    fs::write(root.join(deep.join("f")), "f").unwrap();
    for i in 0..1000 {
        fs::write(root.join(format!("ig/wide/{i}")), "w").unwrap();
        fs::write(root.join(deep.parent().unwrap().join(format!("{i}"))), "w").unwrap();
    }
    fs::write(root.join(".koiignore"), "ig\n").unwrap();
    fs::write(root.join("f.txt"), "f").unwrap();
    let budget = 50;
    let mut rules = Rules::new(Box::new(Lines::default())).with_search_budget(budget);
    let mut snapshot = Snapshot::scan_with_rules(&root, &mut rules).unwrap();
    fs::remove_file(root.join("f.txt")).unwrap();

    let mut paths = vec![PathBuf::new()];
    let mut most = 0;
    for _ in 0..1000 {
        let mut looked = 0;
        let rescan = snapshot
            .rescan_under(
                &root,
                paths,
                &BTreeSet::new(),
                None,
                Some(&mut rules),
                &|| false,
                &|_| {},
                Keep::Unplaced,
                &mut |cut| match cut {
                    Cut::Searching(path) | Cut::OpeningDir(path) if path.starts_with("ig") => {
                        looked += 1;
                    }
                    Cut::Searching(path) => panic!("{path:?}"),
                    Cut::Confirming(_) => looked += 1,
                    _ => {}
                },
                &MarkLog::default(),
            )
            .unwrap();
        most = most.max(looked);
        if !rescan.changes.is_empty() {
            assert_eq!(
                rescan.changes,
                [Change::Removed {
                    path: PathBuf::from("f.txt"),
                    kind: EntryKind::File
                }]
            );
            assert!(most <= budget + 1, "{most} entries and opens in one rescan");
            return;
        }
        paths = rescan.unsettled;
    }
    panic!("the search never ended");
}

#[test]
fn a_left_out_entry_back_while_its_removal_is_confirmed_stays_left_out_until_included() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("sub/hidden"), "known").unwrap();
    let mut rules = Rules::new(Box::new(Lines::default()));
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::write(root.join("sub").join(RULES_FILE), "hidden\n").unwrap();
    snapshot
        .rescan_with_rules(root, [PathBuf::new()], &mut rules)
        .unwrap();
    assert!(snapshot.left_out.contains_key(Path::new("sub/hidden")));
    fs::remove_file(root.join("sub/hidden")).unwrap();
    let mut restored = false;
    let rescan = snapshot
        .rescan_under(
            root,
            [PathBuf::new()],
            &BTreeSet::new(),
            None,
            Some(&mut rules),
            &|| false,
            &|_| {},
            Keep::Unplaced,
            &mut |cut| {
                if !restored && matches!(cut, Cut::Confirming(path) if path == Path::new("sub")) {
                    fs::write(root.join("sub/hidden"), "returned during confirmation").unwrap();
                    restored = true;
                }
            },
            &MarkLog::default(),
        )
        .unwrap();
    assert!(restored);
    assert_eq!(rescan.unsettled, [PathBuf::from("sub/hidden")]);
    assert!(!snapshot.entries.contains_key(Path::new("sub/hidden")));
    assert!(snapshot.left_out.contains_key(Path::new("sub/hidden")));
    fs::write(root.join("sub").join(RULES_FILE), "").unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::new()], &mut rules)
        .unwrap();
    assert!(
        rescan
            .changes
            .contains(&created("sub/hidden", EntryKind::File)),
        "{rescan:?}"
    );
}

#[test]
fn a_mount_point_that_cannot_be_opened_is_left_out_once_its_folder_rules_leave_it_out() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let image = disks.image("Locked");
    fs::create_dir_all(root.join("Disk")).unwrap();
    disks.attach(&image, &root.join("Disk"));
    fs::write(root.join("Disk/report.docx"), "kept").unwrap();
    let mut rules = Rules::new(Box::new(Lines::default()));
    let mut snapshot = Snapshot::scan_with_rules(&root, &mut rules).unwrap();
    fs::write(root.join(RULES_FILE), "Disk\n").unwrap();
    fs::set_permissions(root.join("Disk"), fs::Permissions::from_mode(0o0)).unwrap();
    let rescan = snapshot.rescan_with_rules(&root, [PathBuf::new()], &mut rules);
    fs::set_permissions(root.join("Disk"), fs::Permissions::from_mode(0o755)).unwrap();
    rescan.unwrap();
    let below: Vec<_> = (snapshot.entries.keys())
        .filter(|path| path.starts_with("Disk"))
        .collect();
    assert_eq!(below, Vec::<&PathBuf>::new());
    assert!(snapshot.left_out.contains_key(Path::new("Disk")));
}
