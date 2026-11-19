// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_scoped_rescan_finds_a_hard_link_outside_its_scope_as_a_full_one_does() {
    for paths in [whole().to_vec(), vec![PathBuf::from("a")]] {
        let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
        let root = dir.path();
        write(root, "a", "a");
        fs::create_dir(root.join("other")).unwrap();
        fs::hard_link(root.join("a"), root.join("other/b")).unwrap();
        fs::hard_link(root.join("a"), root.join("ig/link")).unwrap();
        snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        let id = snapshot.entries[Path::new("a")].id;
        fs::remove_file(root.join("a")).unwrap();

        let rescan = snapshot
            .rescan_with_rules(root, paths.clone(), &mut rules)
            .unwrap();
        assert_eq!(
            rescan.changes,
            [Change::Ignored {
                from: "a".into(),
                to: "ig/link".into(),
                kind: File,
                id,
            }],
            "{paths:?}"
        );
    }
}

#[test]
fn a_link_moved_into_an_ignored_folder_while_another_link_stays_included_is_ignored() {
    for (scope, budget) in [
        (whole().to_vec(), usize::MAX),
        (vec![PathBuf::from("f.txt")], usize::MAX),
        (whole().to_vec(), 100),
    ] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "f.txt", "f");
        fs::hard_link(root.join("f.txt"), root.join("also.txt")).unwrap();
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = snapshot.entries[Path::new("f.txt")].id;
        fs::rename(root.join("f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();

        let mut paths = scope.clone();
        let mut changes = Vec::new();
        for _ in 0..20 {
            let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            changes.extend(rescan.changes);
            if rescan.unsettled.is_empty() {
                break;
            }
            paths = rescan.unsettled;
        }
        assert_eq!(
            changes,
            [Change::Ignored {
                from: "f.txt".into(),
                to: "ig/zz/deep/f.txt".into(),
                kind: File,
                id,
            }],
            "{scope:?} {budget}"
        );
        assert!(snapshot.entries.contains_key(Path::new("also.txt")));
    }
}

#[test]
fn a_link_moved_into_an_ignored_folder_while_another_link_is_renamed_is_ignored() {
    for (renamed, ignored, over) in [
        ("also.txt", "f.txt", false),
        ("f.txt", "also.txt", false),
        ("also.txt", "f.txt", true),
    ] {
        for budget in [usize::MAX, 100] {
            let dir = large_ignored_tree();
            let root = dir.path();
            write(root, "f.txt", "f");
            if over {
                write(root, "other.txt", "o");
            }
            fs::hard_link(root.join("f.txt"), root.join("also.txt")).unwrap();
            let mut rules = rules().with_search_budget(budget);
            let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
            fs::rename(root.join(renamed), root.join("other.txt")).unwrap();
            fs::rename(root.join(ignored), root.join("ig/zz/deep/moved.txt")).unwrap();

            let mut paths = whole().to_vec();
            let mut changes = Vec::new();
            for _ in 0..20 {
                let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
                changes.extend(rescan.changes);
                if rescan.unsettled.is_empty() {
                    break;
                }
                paths = rescan.unsettled;
            }
            let case = format!("{renamed} {ignored} over={over} budget={budget}: {changes:?}");
            assert!(
                (changes.iter()).any(|change| matches!(change, Change::Ignored { to, .. }
                    if to == Path::new("ig/zz/deep/moved.txt"))),
                "{case}"
            );
            let removals = (changes.iter())
                .filter(|change| matches!(change, Change::Removed { .. }))
                .count();
            assert_eq!(removals, usize::from(over), "{case}");
        }
    }
}

#[test]
fn two_links_moved_into_an_ignored_folder_beside_a_new_included_link_are_not_removed() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    write(root, "a", "a");
    fs::hard_link(root.join("a"), root.join("b")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("a"), root.join("ig/a")).unwrap();
    fs::rename(root.join("b"), root.join("ig/b")).unwrap();
    fs::hard_link(root.join("ig/a"), root.join("c")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(
        (rescan.changes.iter()).any(|change| matches!(change, Change::Ignored { .. })),
        "{:?}",
        rescan.changes
    );
    assert!(
        !(rescan.changes.iter()).any(|change| matches!(change, Change::Removed { .. })),
        "{:?}",
        rescan.changes
    );
}

#[test]
fn a_file_moved_over_another_with_an_older_link_in_an_ignored_folder_is_removed() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    write(root, "a", "a");
    write(root, "x", "x");
    fs::hard_link(root.join("a"), root.join("ig/l")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("a"), root.join("x")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [removed("a", File), Change::Modified { path: "x".into() }]
    );
}

#[test]
fn a_file_moved_over_another_out_of_a_removed_folder_with_an_older_link_in_an_ignored_folder_is_removed()
 {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    write(root, "x", "x");
    fs::hard_link(root.join("d/g.txt"), root.join("ig/l")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("d/g.txt"), root.join("x")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [removed("d", Dir), Change::Modified { path: "x".into() }]
    );
}

#[test]
fn a_link_removed_while_another_link_stays_included_is_removed() {
    for budget in [usize::MAX, 100] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "f.txt", "f");
        fs::hard_link(root.join("f.txt"), root.join("also.txt")).unwrap();
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        fs::remove_file(root.join("f.txt")).unwrap();

        let mut paths = whole().to_vec();
        let mut changes = Vec::new();
        for _ in 0..20 {
            let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            changes.extend(rescan.changes);
            if rescan.unsettled.is_empty() {
                break;
            }
            paths = rescan.unsettled;
        }
        let modified = Change::Modified {
            path: "also.txt".into(),
        };
        assert_eq!(changes.len(), 2, "{budget}: {changes:?}");
        assert!(changes.contains(&removed("f.txt", File)), "{budget}");
        assert!(changes.contains(&modified), "{budget}");
        assert!(snapshot.entries.contains_key(Path::new("also.txt")));
    }
}

#[test]
fn a_file_moved_out_of_a_removed_folder_with_a_link_in_an_ignored_folder_is_renamed() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    fs::hard_link(root.join("d/g.txt"), root.join("ig/alias.txt")).unwrap();
    fs::rename(root.join("d/g.txt"), root.join("moved.txt")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let renamed = |change: &Change| {
        matches!(change, Change::Renamed { from, to, .. }
            if from == Path::new("d/g.txt") && to == Path::new("moved.txt"))
    };
    let ignored = |change: &Change| matches!(change, Change::Ignored { from, .. } if from == Path::new("d/g.txt"));
    assert!(rescan.changes.iter().any(renamed), "{:?}", rescan.changes);
    assert!(!rescan.changes.iter().any(ignored), "{:?}", rescan.changes);
}

#[test]
fn a_file_moved_out_of_a_removed_folder_is_renamed_and_its_link_moved_into_an_ignored_folder_is_ignored()
 {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    fs::hard_link(root.join("d/g.txt"), root.join("d/h.txt")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let id = snapshot.entries[Path::new("d/g.txt")].id;
    fs::rename(root.join("d/g.txt"), root.join("moved.txt")).unwrap();
    fs::rename(root.join("d/h.txt"), root.join("ig/h.txt")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            Change::Removed {
                path: "d".into(),
                kind: Dir,
            },
            Change::Renamed {
                from: "d/g.txt".into(),
                to: "moved.txt".into(),
                kind: File,
            },
            Change::Ignored {
                from: "d/h.txt".into(),
                to: "ig/h.txt".into(),
                kind: File,
                id,
            },
        ]
    );
}

#[test]
fn a_file_in_a_folder_renamed_inside_a_renamed_folder_with_a_link_in_an_ignored_folder_is_not_ignored()
 {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, KOIIGNORE, "ig/\n");
    write(root, "d/a/x/file", "kept");
    fs::create_dir(root.join("ig")).unwrap();
    fs::create_dir(root.join("dest")).unwrap();
    let mut snapshot = Snapshot::default();
    let mut rules = rules();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::hard_link(root.join("d/a/x/file"), root.join("ig/alias")).unwrap();
    fs::rename(root.join("d/a"), root.join("dest/a")).unwrap();
    fs::rename(root.join("dest/a/x"), root.join("dest/a/y")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            Change::Removed {
                path: "d".into(),
                kind: Dir,
            },
            Change::Renamed {
                from: "d/a".into(),
                to: "dest/a".into(),
                kind: Dir,
            },
            Change::Renamed {
                from: "d/a/x".into(),
                to: "dest/a/y".into(),
                kind: Dir,
            },
        ]
    );
    assert!(snapshot.entries.contains_key(Path::new("dest/a/y/file")));
}

#[test]
fn a_file_moved_into_an_ignored_folder_out_of_a_removed_folder_with_an_included_link_that_stays_is_ignored()
 {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    fs::hard_link(root.join("d/g.txt"), root.join("also.txt")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("d/g.txt"), root.join("ig/g.txt")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let ignored = |change: &Change| {
        matches!(change, Change::Ignored { from, to, .. }
            if from == Path::new("d/g.txt") && to == Path::new("ig/g.txt"))
    };
    assert!(rescan.changes.iter().any(ignored), "{:?}", rescan.changes);
    assert!(snapshot.entries.contains_key(Path::new("also.txt")));
}

#[test]
fn a_file_saved_by_rename_with_a_link_below_an_ignored_folder_is_modified() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, KOIIGNORE, "ig/\n");
    write(root, "f.txt", "f");
    fs::create_dir_all(root.join("ig/snap")).unwrap();
    fs::hard_link(root.join("f.txt"), root.join("ig/snap/f.txt")).unwrap();
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    save(root, "f.txt", "saved");

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Modified {
            path: "f.txt".into(),
        }]
    );
}

#[test]
fn a_file_saved_by_rename_with_a_link_at_an_ignored_name_is_modified() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, KOIIGNORE, "*.bak\n");
    write(root, "f.txt", "f");
    fs::hard_link(root.join("f.txt"), root.join("f.bak")).unwrap();
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    save(root, "f.txt", "saved");

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Modified {
            path: "f.txt".into(),
        }]
    );
}
