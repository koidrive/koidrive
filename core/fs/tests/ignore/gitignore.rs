// SPDX-License-Identifier: Apache-2.0

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
