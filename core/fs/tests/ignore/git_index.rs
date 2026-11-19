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

#[test]
fn an_outer_index_answers_nothing_below_a_nested_work_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "*.env\n");
    write(root, "inner/secret.env", "secret");
    write(root, "inner/a.txt", "a");
    git(root, &["init", "-q"]);
    git(root, &["add", "-f", "inner/secret.env"]);
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let mut scan = || {
        snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        known(&snapshot).contains(&"inner/secret.env")
    };
    assert!(scan());
    std::os::unix::fs::symlink("/nonexistent", root.join("inner/.git")).unwrap();
    assert!(!scan(), "a .git link");
    fs::remove_file(root.join("inner/.git")).unwrap();
    git(&root.join("inner"), &["init", "-q"]);
    git(&root.join("inner"), &["add", "a.txt"]);
    assert!(!scan(), "an inner index that does not list it");
    fs::remove_dir_all(root.join("inner/.git")).unwrap();
    assert!(scan(), "no inner work tree");
}

#[test]
fn a_watcher_reads_an_index_outside_the_root_only_after_an_event_for_it_or_its_lock() {
    let main = tempfile::tempdir().unwrap();
    write(main.path(), ".gitignore", "dist/\n");
    git(main.path(), &["init", "-q"]);
    git(main.path(), &["add", "."]);
    git(main.path(), &["commit", "-qm", "main"]);
    let dir = tempfile::tempdir().unwrap();
    let root = &dir.path().join("wt");
    git(
        main.path(),
        &["worktree", "add", "-q", root.to_str().unwrap()],
    );
    write(root, "dist/app.js", "a");
    write(root, "dist/b.js", "b");
    git(root, &["add", "-f", "dist/app.js"]);
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let mut watcher = NativeWatcher::start_with_rules(root, snapshot, rules).unwrap();
    let read = |watcher: &mut NativeWatcher, wait: Duration| {
        let mut read = Vec::new();
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline && !known(watcher.snapshot()).contains(&"dist/b.js") {
            read.extend(watcher.poll(Duration::from_millis(300)).unwrap().read);
        }
        read
    };
    read(&mut watcher, Duration::from_secs(1));
    let git_dir = main.path().join(".git/worktrees/wt");
    for n in 0..5 {
        write(&git_dir, &format!("noise{n}"), "x");
    }
    assert_eq!(
        read(&mut watcher, Duration::from_secs(2)),
        Vec::<PathBuf>::new()
    );
    let next = git_dir.join("next-index");
    fs::copy(git_dir.join("index"), &next).unwrap();
    let out = std::process::Command::new("git")
        .current_dir(root)
        .env("GIT_INDEX_FILE", &next)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["add", "-f", "dist/b.js"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    fs::rename(&next, git_dir.join("index")).unwrap();
    fs::write(git_dir.join("index.lock"), "").unwrap();
    read(&mut watcher, Duration::from_millis(1500));
    fs::remove_file(git_dir.join("index.lock")).unwrap();
    read(&mut watcher, WAIT);
    assert!(known(watcher.snapshot()).contains(&"dist/b.js"));
}
