// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_root_rule_leaves_out_a_file_and_a_folder_with_everything_below_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, KOIIGNORE, "*.log\nbuild/\n");
    write(root, "a.log", "a");
    write(root, "keep.txt", "k");
    write(root, "build/out/x.txt", "x");
    write(root, "src/main.rs", "m");
    write(root, "src/b.log", "b");
    write(root, "src/build/y.txt", "y");

    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        known(&snapshot),
        [KOIIGNORE, "keep.txt", "src", "src/main.rs"]
    );
    assert_eq!(
        rescan.changes,
        [
            created(KOIIGNORE, File),
            created("keep.txt", File),
            created("src", Dir),
            created("src/main.rs", File),
        ]
    );
    assert_eq!(rescan.rules, [rule("", Some("*.log\nbuild/\n"))]);
    assert_eq!(
        snapshot,
        Snapshot::scan_with_rules(root, &mut self::rules()).unwrap()
    );
    assert_eq!(
        rules.status(Path::new("build/out/x.txt"), File),
        Ignored::Rule
    );
}

#[test]
fn a_nested_rules_file_applies_to_its_folder_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "docs/.koiignore", "draft\n");
    write(root, "docs/draft", "d");
    write(root, "docs/deep/draft/x", "x");
    write(root, "docs/final", "f");
    write(root, "other/draft", "o");

    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        known(&snapshot),
        [
            "docs",
            "docs/.koiignore",
            "docs/deep",
            "docs/final",
            "other",
            "other/draft"
        ]
    );
    assert_eq!(rescan.rules, [rule("docs", Some("draft\n"))]);
}

#[test]
fn removing_a_rule_reports_the_entries_as_created_also_deep_below_the_rules_file() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, KOIIGNORE, "*.tmp\ncache/\n");
    snapshot
        .rescan_with_rules(root, [PathBuf::from(KOIIGNORE)], &mut rules)
        .unwrap();

    write(root, KOIIGNORE, "cache/\n");
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(KOIIGNORE)], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            created("c.tmp", File),
            created("sub/a.tmp", File),
            Change::Modified {
                path: KOIIGNORE.into()
            },
        ]
    );
    assert_eq!(rescan.rules, [rule("", Some("cache/\n"))]);

    fs::remove_file(root.join(KOIIGNORE)).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(KOIIGNORE)], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            removed(KOIIGNORE, File),
            created("sub/cache", Dir),
            created("sub/cache/x", Dir),
            created("sub/cache/x/y.txt", File),
        ]
    );
    assert_eq!(rescan.rules, [rule("", None)]);
    assert_eq!(snapshot, Snapshot::scan(root).unwrap());
}

#[test]
fn rules_inside_a_folder_that_becomes_left_out_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "ig/.koiignore", "x\n");
    write(root, "ig/x", "x");
    write(root, "ig/y", "y");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, [rule("ig", Some("x\n"))]);
    assert_eq!(known(&snapshot), ["ig", "ig/.koiignore", "ig/y"]);

    write(root, KOIIGNORE, "ig/\n");
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(KOIIGNORE)], &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [created(KOIIGNORE, File)]);
    assert_eq!(rescan.rules, [rule("", Some("ig/\n")), rule("ig", None)]);
    assert_eq!(known(&snapshot), [KOIIGNORE]);

    fs::remove_file(root.join(KOIIGNORE)).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(KOIIGNORE)], &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, [rule("", None), rule("ig", Some("x\n"))]);
    assert_eq!(known(&snapshot), ["ig", "ig/.koiignore", "ig/y"]);
}

#[test]
fn a_symbolic_link_or_a_folder_named_like_a_rules_file_is_not_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    write(dir.path(), "outside-rules", "x\n");
    write(&root, "linked/x", "x");
    write(&root, "folder/.koiignore/x", "x");
    write(&root, "folder/x", "x");
    std::os::unix::fs::symlink(
        dir.path().join("outside-rules"),
        root.join("linked/.koiignore"),
    )
    .unwrap();

    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(&root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, []);
    assert_eq!(
        known(&snapshot),
        [
            "folder",
            "folder/.koiignore",
            "folder/.koiignore/x",
            "folder/x",
            "linked",
            "linked/x"
        ]
    );
}

#[test]
fn a_rules_file_over_the_size_limit_keeps_the_rules_the_folder_had() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, KOIIGNORE, "x\n");
    write(root, "x", "x");
    write(root, "y", "y");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(known(&snapshot), [KOIIGNORE, "y"]);

    write(root, KOIIGNORE, &"y\n".repeat((1 << 19) + 1));
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Modified {
            path: KOIIGNORE.into()
        }]
    );
    assert_eq!(rescan.rules, []);
    assert_eq!(known(&snapshot), [KOIIGNORE, "y"]);

    write(root, KOIIGNORE, &"y\n".repeat(1 << 19));
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules.len(), 1);
    assert_eq!(known(&snapshot), [KOIIGNORE, "x"]);
}

#[test]
fn built_in_platform_files_are_left_out_and_are_not_ignored_entries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, KOIIGNORE, "!.DS_Store\n");
    write(root, ".DS_Store", "finder");
    write(root, "d/Thumbs.db/inside", "t");
    write(root, "d/x.txt", "x");
    write(root, "gone.txt", "g");
    write(root, "moved.txt", "m");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(
        known(&snapshot),
        [KOIIGNORE, "d", "d/x.txt", "gone.txt", "moved.txt"]
    );
    assert_eq!(rules.status(Path::new(".DS_Store"), File), Ignored::BuiltIn);

    fs::remove_file(root.join("gone.txt")).unwrap();
    fs::rename(root.join("moved.txt"), root.join("d/Thumbs.db/moved.txt")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [removed("gone.txt", File), removed("moved.txt", File)]
    );
}

#[test]
fn a_walk_under_way_looks_in_a_folder_the_rules_leave_out_since_it_began() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "f.txt", "f");
    write(root, "ig2/keep.txt", "k");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    let id = snapshot.entries[Path::new("f.txt")].id;
    fs::rename(root.join("f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unsettled, [PathBuf::from("f.txt")]);
    write(root, KOIIGNORE, "ig/\nig2/\n");
    fs::rename(root.join("ig/zz/deep/f.txt"), root.join("ig2/f.txt")).unwrap();

    let mut changes = Vec::new();
    for _ in 0..10 {
        let rescan = snapshot.rescan_with_rules(root, whole(), &mut rules);
        changes.extend(rescan.unwrap().changes);
    }
    assert!(
        changes.contains(&Change::Ignored {
            from: "f.txt".into(),
            to: "ig2/f.txt".into(),
            kind: File,
            id,
        }),
        "{changes:?}"
    );
}

#[test]
fn renaming_a_folder_that_holds_a_rules_file_moves_its_rules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a/.koiignore", "x\n");
    write(root, "a/x", "x");
    write(root, "a/y", "y");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(known(&snapshot), ["a", "a/.koiignore", "a/y"]);

    fs::rename(root.join("a"), root.join("b")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Renamed {
            from: "a".into(),
            to: "b".into(),
            kind: Dir,
        }]
    );
    assert_eq!(rescan.rules, [rule("a", None), rule("b", Some("x\n"))]);
    assert_eq!(known(&snapshot), ["b", "b/.koiignore", "b/y"]);

    write(root, "a/x", "x");
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from("a")], &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [created("a", Dir), created("a/x", File)]);
    assert_eq!(rescan.rules, []);
}

#[test]
fn scans_without_rules_leave_out_nothing_but_the_private_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, KOIIGNORE, "ig/\n");
    write(root, "ig/x", "x");
    write(root, ".DS_Store", "finder");
    write(root, ".koidrive/space.json", "{}");
    let mut snapshot = Snapshot::default();
    let rescan = snapshot.rescan(root, whole()).unwrap();
    assert_eq!(known(&snapshot), [".DS_Store", KOIIGNORE, "ig", "ig/x"]);
    assert_eq!(rescan.rules, []);
}

#[test]
fn scans_leave_out_what_systems_write_at_a_volume_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let written = [
        ".fseventsd/fseventsd-uuid",
        ".Spotlight-V100/Store-V2/index",
        ".Trashes/501/report.docx",
        ".TemporaryItems/folders.501/x",
        ".DocumentRevisions-V100/db-V1/db.sqlite",
        ".PKInstallSandboxManager/sandbox/x",
        ".PKInstallSandboxManager-SystemSoftware/sandbox/x",
        ".com.apple.timemachine.donotpresent",
        ".com.apple.timemachine.supported",
        ".HFS+ Private Directory Data\r/dir_16",
        "System Volume Information/IndexerVolumeGuid",
        "$RECYCLE.BIN/S-1-5-21-1/desktop.ini",
        "$Recycle.Bin/S-1-5-21-2/$RABC.docx",
    ];
    write(root, KOIIGNORE, "!*\n");
    write(root, "Disk/report.docx", "mine");
    for path in written {
        write(root, path, "system");
        write(root, &format!("Disk/{path}"), "system");
    }
    let mut rules = rules();
    let snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(known(&snapshot), [KOIIGNORE, "Disk", "Disk/report.docx"]);
}
