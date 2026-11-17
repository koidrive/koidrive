// SPDX-License-Identifier: Apache-2.0

#[test]
fn adding_a_gitignore_rule_drops_the_matching_entries_without_a_removal() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, GITIGNORE, "*.tmp\ncache/\n");

    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(GITIGNORE)], &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [created(GITIGNORE, File)]);
    assert_eq!(rescan.rules, [git_rule("", Some("*.tmp\ncache/\n"))]);
    assert_eq!(rescan.unsettled, Vec::<PathBuf>::new());
    assert_eq!(known(&snapshot), [GITIGNORE, "sub", "sub/b.txt"]);
    assert!(root.join("sub/cache/x/y.txt").exists());
}

#[test]
fn removing_a_gitignore_rule_reports_the_entries_as_created() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, GITIGNORE, "*.tmp\ncache/\n");
    snapshot
        .rescan_with_rules(root, [PathBuf::from(GITIGNORE)], &mut rules)
        .unwrap();

    write(root, GITIGNORE, "cache/\n");
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(GITIGNORE)], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            created("c.tmp", File),
            created("sub/a.tmp", File),
            Change::Modified {
                path: GITIGNORE.into()
            },
        ]
    );
    assert_eq!(rescan.rules, [git_rule("", Some("cache/\n"))]);

    fs::remove_file(root.join(GITIGNORE)).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(GITIGNORE)], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            removed(GITIGNORE, File),
            created("sub/cache", Dir),
            created("sub/cache/x", Dir),
            created("sub/cache/x/y.txt", File),
        ]
    );
    assert_eq!(rescan.rules, [git_rule("", None)]);
    assert_eq!(snapshot, Snapshot::scan(root).unwrap());
}

#[test]
fn a_folder_with_both_rules_files_reads_and_reports_each_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "sub/.koiignore", "a\n!b\n");
    write(root, "sub/.gitignore", "b\nc\n");
    for name in ["a", "b", "c", "d"] {
        write(root, &format!("sub/{name}"), name);
    }
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.rules,
        [
            rule("sub", Some("a\n!b\n")),
            git_rule("sub", Some("b\nc\n"))
        ]
    );
    assert_eq!(
        known(&snapshot),
        ["sub", "sub/.gitignore", "sub/.koiignore", "sub/b", "sub/d"]
    );

    write(root, "sub/.gitignore", "d\n");
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, [git_rule("sub", Some("d\n"))]);
    assert_eq!(
        known(&snapshot),
        ["sub", "sub/.gitignore", "sub/.koiignore", "sub/b", "sub/c"]
    );

    fs::remove_file(root.join("sub/.koiignore")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, [rule("sub", None)]);
    assert_eq!(
        known(&snapshot),
        ["sub", "sub/.gitignore", "sub/a", "sub/b", "sub/c"]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn two_gitignore_files_that_differ_only_in_case_leave_the_folder_unknown_and_the_koiignore_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "sub/.koiignore", "a\n");
    write(root, "sub/.gitignore", "b\n");
    write(root, "sub/.GITIGNORE", "c\n");
    write(root, "sub/c", "left out by .GITIGNORE only");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unreadable, [PathBuf::from("sub")]);
    assert_eq!(first.rules, []);
    assert_eq!(known(&snapshot), ["sub"]);

    fs::remove_file(root.join("sub/.GITIGNORE")).unwrap();
    let resolved = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(resolved.unreadable.is_empty());
    assert_eq!(
        resolved.rules,
        [rule("sub", Some("a\n")), git_rule("sub", Some("b\n"))]
    );
    assert_eq!(
        known(&snapshot),
        ["sub", "sub/.gitignore", "sub/.koiignore", "sub/c"]
    );
}

#[test]
fn native_watcher_reports_a_gitignore_change() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "node_modules/pkg/index.js", "i");
    write(&root, "keep.txt", "k");
    let mut watcher = NativeWatcher::start_with_rules(&root, Snapshot::default(), rules()).unwrap();
    expect(
        &mut watcher,
        &[
            created("keep.txt", File),
            created("node_modules/pkg/index.js", File),
        ],
        &[],
    );

    write(&root, GITIGNORE, "node_modules/\n");
    let polls = expect(
        &mut watcher,
        &[created(GITIGNORE, File)],
        &[git_rule("", Some("node_modules/\n"))],
    );
    let changes: Vec<&Change> = polls.iter().flat_map(|poll| &poll.changes).collect();
    assert_eq!(changes, [&created(GITIGNORE, File)]);
    assert_eq!(known(watcher.snapshot()), [GITIGNORE, "keep.txt"]);

    write(&root, GITIGNORE, "");
    expect(
        &mut watcher,
        &[
            created("node_modules", Dir),
            created("node_modules/pkg/index.js", File),
        ],
        &[git_rule("", Some(""))],
    );
}
