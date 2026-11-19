// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_held_entry_under_a_folder_that_cannot_be_read_is_not_removed() {
    use std::os::unix::fs::PermissionsExt;
    for left_out in [false, true] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/x", "x");
        write(root, "d/y", "y");
        let mut rules = rules().with_search_budget(100);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        fs::rename(root.join("d/x"), root.join("ig/zz/deep/x")).unwrap();
        let first = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        assert_eq!(first.unsettled, [PathBuf::from("d/x")]);
        fs::rename(root.join("ig/zz/deep/x"), root.join("d/x")).unwrap();
        fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o000)).unwrap();
        if left_out {
            write(root, KOIIGNORE, "ig/\nd/\n");
        }

        let mut changes = Vec::new();
        for _ in 0..10 {
            let rescan = snapshot.rescan_with_rules(root, whole(), &mut rules);
            changes.extend(rescan.unwrap().changes);
        }
        fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            !changes
                .iter()
                .any(|change| matches!(change, Change::Removed { .. })),
            "{left_out} {changes:?}"
        );
    }
}

#[test]
fn a_rules_file_that_cannot_be_read_at_the_first_scan_leaves_its_folder_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "keep.txt", "k");
    write(root, "sub/.koiignore", &"x\n".repeat((1 << 19) + 1));
    write(root, "sub/x", "left out");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.unreadable, [PathBuf::from("sub")]);
    assert_eq!(rescan.rules, []);
    assert_eq!(known(&snapshot), ["keep.txt", "sub"]);

    write(root, "sub/.koiignore", "x\n");
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.unreadable, Vec::<PathBuf>::new());
    assert_eq!(rescan.rules, [rule("sub", Some("x\n"))]);
    assert_eq!(rescan.changes, [created("sub/.koiignore", File)]);
    assert_eq!(known(&snapshot), ["keep.txt", "sub", "sub/.koiignore"]);
}

#[test]
fn a_folder_whose_rules_were_unknown_is_reported_once_they_are_known() {
    for how in ["the rules file is deleted", "the folder is deleted"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "sub/.koiignore", &"x\n".repeat((1 << 19) + 1));
        write(root, "sub/x", "kept");
        let mut rules = rules();
        let mut snapshot = Snapshot::default();
        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        assert_eq!(rescan.unreadable, [PathBuf::from("sub")], "{how}");
        assert_eq!(rescan.read, [PathBuf::new()], "{how}");

        let sub = root.join("sub");
        match how {
            "the folder is deleted" => fs::remove_dir_all(sub).unwrap(),
            _ => fs::remove_file(sub.join(".koiignore")).unwrap(),
        }
        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        assert_eq!(rescan.unreadable, Vec::<PathBuf>::new(), "{how}");
        assert_eq!(rescan.rules, [], "{how}");
        let read: &[&str] = match how {
            "the folder is deleted" => &[""],
            _ => &["", "sub"],
        };
        assert_eq!(
            rescan.read,
            read.iter().map(PathBuf::from).collect::<Vec<_>>(),
            "{how}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn reading_a_folder_leaves_its_unread_namesake_in_another_case_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "proj/ok", "ok");
    write(root, "PROJ/.koiignore", &"x\n".repeat((1 << 19) + 1));
    write(root, "PROJ/x", "unknown rules apply here");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unreadable, [PathBuf::from("PROJ")]);
    let partial = (snapshot.rescan_with_rules(root, [PathBuf::from("proj")], &mut rules)).unwrap();
    assert!(partial.unreadable.is_empty());
    assert_eq!(partial.rules, [], "only proj was read");
}

#[cfg(target_os = "linux")]
#[test]
fn a_folder_with_two_rules_files_that_differ_only_in_case_is_unknown_until_one_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "sub/.koiignore", "a\n");
    write(root, "sub/.KOIIGNORE", "b\n");
    write(root, "sub/b", "left out by .KOIIGNORE only");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unreadable, [PathBuf::from("sub")]);
    assert_eq!(first.read, [PathBuf::new()]);
    assert_eq!(known(&snapshot), ["sub"]);

    fs::remove_file(root.join("sub/.KOIIGNORE")).unwrap();
    let resolved = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(resolved.unreadable.is_empty());
    assert_eq!(resolved.read, [PathBuf::new(), PathBuf::from("sub")]);
    assert_eq!(resolved.rules, [rule("sub", Some("a\n"))]);
    assert_eq!(known(&snapshot), ["sub", "sub/.koiignore", "sub/b"]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_folder_named_like_the_rules_file_does_not_hide_a_rules_file_in_another_case() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "sub/.koiignore/x", "x");
    write(root, "sub/.KOIIGNORE", "b\n");
    write(root, "sub/b", "left out by .KOIIGNORE");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(rescan.unreadable.is_empty());
    assert_eq!(rescan.rules, [rule("sub", Some("b\n"))]);
    assert_eq!(
        known(&snapshot),
        [
            "sub",
            "sub/.KOIIGNORE",
            "sub/.koiignore",
            "sub/.koiignore/x"
        ]
    );
}

#[test]
fn review_renaming_an_unread_folder_and_removing_its_rules_reports_the_current_path_known() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "proj/.koiignore", &"x\n".repeat((1 << 19) + 1));
    write(root, "proj/ok", "ok");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unreadable, [PathBuf::from("proj")]);
    fs::rename(root.join("proj"), root.join("PROJ")).unwrap();
    fs::remove_file(root.join("PROJ/.koiignore")).unwrap();
    let recovered = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(recovered.unreadable.is_empty());
    assert!(
        recovered.read.iter().any(|dir| dir == Path::new("PROJ")),
        "current folder never reported known: {:?}",
        recovered.read
    );
}

#[test]
fn a_folder_moved_over_an_empty_folder_with_unreadable_rules_keeps_its_entries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "drafts/.koiignore", "*.tmp\n");
    write(root, "drafts/one.txt", "one");
    write(root, "drafts/inner/two.txt", "two");
    fs::create_dir(root.join("final")).unwrap();
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    write(root, "drafts/.koiignore", &"x\n".repeat((1 << 19) + 1));
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.unreadable, Vec::<PathBuf>::new());

    fs::rename(root.join("drafts"), root.join("final")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            removed("final", Dir),
            Change::Renamed {
                from: "drafts".into(),
                to: "final".into(),
                kind: Dir,
            },
        ]
    );
    assert_eq!(rescan.unreadable, [PathBuf::from("final")]);
    let moved = [
        "final",
        "final/.koiignore",
        "final/inner",
        "final/inner/two.txt",
        "final/one.txt",
    ];
    assert_eq!(known(&snapshot), moved);

    write(root, "final/.koiignore", "*.tmp\n");
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Modified {
            path: "final/.koiignore".into()
        }]
    );
    assert_eq!(rescan.unreadable, Vec::<PathBuf>::new());
    assert_eq!(known(&snapshot), moved);
}
