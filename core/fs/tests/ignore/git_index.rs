// SPDX-License-Identifier: Apache-2.0

fn git(root: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["-c", "user.name=koi", "-c", "user.email=koi@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn shown(tracked: &[(PathBuf, Vec<String>)]) -> String {
    let tree = |(tree, paths): &(PathBuf, Vec<String>)| format!("{tree:?}:{paths:?}");
    tracked.iter().map(tree).collect::<Vec<_>>().join(" ")
}

#[test]
fn a_watcher_scans_the_files_a_git_index_lists_below_a_folder_a_gitignore_excludes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "dist/\n");
    for path in ["dist/app.js", "dist/other.js", "dist/sub/deep.js"] {
        write(root, path, path);
    }
    write(root, "repo/dist/x.js", "x");
    git(root, &["init", "-q"]);
    git(root, &["add", "-f", "dist/app.js"]);
    git(&root.join("repo"), &["init", "-q"]);
    git(&root.join("repo"), &["add", "-f", "dist/x.js"]);
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let scan = snapshot.rescan_with_rules(root, whole(), &mut rules);
    let expected = ".gitignore dist dist/app.js repo repo/dist repo/dist/x.js";
    assert_eq!(known(&snapshot).join(" "), expected);
    let both = r#""":["dist/app.js"] "repo":["dist/x.js"]"#;
    assert_eq!(shown(&scan.unwrap().tracked), both);

    let mut watcher = NativeWatcher::start_with_rules(root, snapshot, rules).unwrap();
    let settle = |watcher: &mut NativeWatcher| {
        let (mut changes, mut tracked) = (Vec::new(), Vec::new());
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            let poll = watcher.poll(Duration::from_millis(300)).unwrap();
            let quiet = poll.changes.is_empty() && poll.tracked.is_empty();
            changes.extend(poll.changes);
            tracked.extend(poll.tracked);
            if quiet && poll.pending.is_empty() && !(changes.is_empty() && tracked.is_empty()) {
                break;
            }
        }
        (changes, shown(&tracked))
    };
    git(root, &["add", "-f", "dist/sub/deep.js"]);
    let deep = vec![created("dist/sub", Dir), created("dist/sub/deep.js", File)];
    let now = r#""":["dist/app.js", "dist/sub/deep.js"]"#;
    assert_eq!(settle(&mut watcher), (deep, now.into()));
    git(root, &["rm", "-q", "--cached", "dist/app.js"]);
    let now = r#""":["dist/sub/deep.js"]"#;
    assert_eq!(settle(&mut watcher), (Vec::new(), now.into()));
    assert!(!known(watcher.snapshot()).contains(&"dist/app.js"));
    assert!(root.join("dist/app.js").exists());
    fs::rename(root.join("repo"), root.join("moved")).unwrap();
    let now = r#""moved":["dist/x.js"] "repo":[]"#;
    assert_eq!(settle(&mut watcher).1, now);
    assert!(known(watcher.snapshot()).contains(&"moved/dist/x.js"));
}
