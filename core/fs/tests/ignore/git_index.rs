// SPDX-License-Identifier: Apache-2.0

fn git(root: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .env_clear()
        .envs(std::env::vars_os().filter(|(name, _)| !name.to_string_lossy().starts_with("GIT_")))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
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
    fs::rename(root.join("dist/sub/deep.js"), root.join("dist/sub/Deep.js")).unwrap();
    settle(&mut watcher);
    git(root, &["rm", "-q", "--cached", "dist/sub/deep.js"]);
    settle(&mut watcher);
    let left = known(watcher.snapshot());
    assert!(!left.iter().any(|path| path.starts_with("dist")));
    fs::rename(root.join("repo"), root.join("moved")).unwrap();
    let now = r#""moved":["dist/x.js"] "repo":[]"#;
    assert_eq!(settle(&mut watcher).1, now);
    assert!(known(watcher.snapshot()).contains(&"moved/dist/x.js"));
    fs::rename(root.join("moved"), root.join("Moved")).unwrap();
    assert_eq!(settle(&mut watcher).1, r#""Moved":["dist/x.js"]"#);
    assert!(known(watcher.snapshot()).contains(&"Moved/dist/x.js"));
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
    let mut scan = |path| {
        snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        known(&snapshot).contains(&path)
    };
    assert!(scan("inner/secret.env"));
    std::os::unix::fs::symlink("/nonexistent", root.join("inner/.git")).unwrap();
    assert!(!scan("inner/secret.env"), "a .git link");
    fs::remove_file(root.join("inner/.git")).unwrap();
    git(&root.join("inner"), &["init", "-q"]);
    git(&root.join("inner"), &["add", "a.txt"]);
    assert!(!scan("inner/secret.env"), "an inner index without it");
    fs::remove_dir_all(root.join("inner/.git")).unwrap();
    assert!(scan("inner/secret.env"), "no inner work tree");
    write(root, ".gitignore", "*.env\ninner/\n");
    git(&root.join("inner"), &["init", "-q"]);
    let opened = [scan("inner"), scan("inner"), scan("inner")];
    assert_eq!(opened, [true, false, false], "opened by the outer index");
    fs::remove_dir_all(root.join("inner/.git")).unwrap();
    let gone = [scan("inner"), scan("inner/secret.env")];
    assert_eq!(gone, [false, true], "its work tree is gone");
}

#[test]
fn a_watcher_scans_the_files_an_outer_index_lists_once_a_left_out_work_tree_is_gone() {
    for init in ["-q", "--separate-git-dir=../.git/inner"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, ".gitignore", "*.env\ninner/\n");
        write(root, "inner/secret.env", "secret");
        git(root, &["init", "-q"]);
        git(root, &["add", "-f", "inner/secret.env"]);
        git(&root.join("inner"), &["init", "-q", init]);
        let start = NativeWatcher::start_with_rules(root, Snapshot::default(), rules());
        let mut watcher = start.unwrap();
        let found = |w: &NativeWatcher| known(w.snapshot()).contains(&"inner/secret.env");
        watcher.poll(Duration::from_secs(2)).unwrap();
        assert!(watcher.poll(WAIT / 10).is_ok() && !found(&watcher));
        let marker = root.join("inner/.git");
        assert!(fs::remove_file(&marker).is_ok() || fs::remove_dir_all(&marker).is_ok());
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline && !found(&watcher) {
            watcher.poll(Duration::from_millis(300)).unwrap();
        }
        assert!(found(&watcher), "{init}");
    }
}

#[test]
fn a_watcher_reads_a_split_index_again_when_its_shared_index_returns() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "dist/\n");
    write(root, "dist/a.js", "a");
    git(root, &["init", "-q"]);
    git(root, &["add", "-f", "dist/a.js"]);
    git(root, &["update-index", "--split-index"]);
    let shared = (fs::read_dir(root.join(".git")).unwrap())
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().contains("sharedindex."))
        .unwrap();
    let away = tempfile::tempdir().unwrap();
    fs::rename(&shared, away.path().join("shared")).unwrap();
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let mut watcher = NativeWatcher::start_with_rules(root, snapshot, rules).unwrap();
    watcher.poll(Duration::from_secs(2)).unwrap();
    assert!(!known(watcher.snapshot()).contains(&"dist/a.js"));
    fs::rename(away.path().join("shared"), &shared).unwrap();
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline && !known(watcher.snapshot()).contains(&"dist/a.js") {
        watcher.poll(Duration::from_millis(300)).unwrap();
    }
    assert!(known(watcher.snapshot()).contains(&"dist/a.js"));
}

#[cfg(target_os = "linux")]
#[test]
fn a_watcher_keeps_reading_an_index_outside_the_root_it_may_not_watch() {
    use std::os::unix::fs::PermissionsExt;
    let away = tempfile::tempdir().unwrap();
    let git_dir = away.path().join("g");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "dist/\n");
    write(root, "dist/a.js", "a");
    let separate = format!("--separate-git-dir={}", git_dir.display());
    git(root, &["init", "-q", &separate]);
    git(root, &["add", "-f", "dist/a.js"]);
    fs::set_permissions(&git_dir, fs::Permissions::from_mode(0o100)).unwrap();
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let mut watcher = NativeWatcher::start_with_rules(root, snapshot, rules).unwrap();
    let polled = watcher.poll(Duration::from_secs(1)).map(|_| ());
    fs::set_permissions(&git_dir, fs::Permissions::from_mode(0o755)).unwrap();
    polled.unwrap();
    assert!(known(watcher.snapshot()).contains(&"dist/a.js"));
}

#[test]
fn a_watcher_reads_an_index_outside_the_root_after_an_event_for_it_or_its_lock() {
    let main_dir = tempfile::tempdir().unwrap();
    let main = main_dir.path();
    git(main, &["init", "-q"]);
    git(main, &["commit", "-q", "--allow-empty", "-m", "main"]);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let wt = &root.join("wt");
    git(main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    write(root, ".gitignore", "dist/\n");
    write(root, "wt/dist/b.js", "b");
    write(root, "copy/dist/b.js", "b");
    fs::copy(wt.join(".git"), root.join("copy/.git")).unwrap();
    let git_dir = main.join(".git/worktrees/wt");
    git(wt, &["add", "-f", "dist/b.js"]);
    fs::copy(git_dir.join("index"), git_dir.join("next-index")).unwrap();
    git(wt, &["rm", "-q", "--cached", "dist/b.js"]);
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let mut watcher = NativeWatcher::start_with_rules(root, snapshot, rules).unwrap();
    let listed = |watcher: &NativeWatcher| {
        let known = known(watcher.snapshot());
        ["wt/dist/b.js", "copy/dist/b.js"].map(|path| known.contains(&path))
    };
    let read = |watcher: &mut NativeWatcher, wait: Duration| {
        let mut read = Vec::new();
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline && listed(watcher) != [true, true] {
            read.extend(watcher.poll(Duration::from_millis(300)).unwrap().read);
        }
        read
    };
    read(&mut watcher, Duration::from_secs(1));
    for n in 0..5 {
        write(&git_dir, &format!("noise{n}"), "x");
    }
    let noise = read(&mut watcher, Duration::from_secs(2));
    assert_eq!(noise, Vec::<PathBuf>::new());
    fs::write(git_dir.join("index.lock"), "").unwrap();
    fs::rename(git_dir.join("next-index"), git_dir.join("index")).unwrap();
    read(&mut watcher, Duration::from_millis(1500));
    assert_eq!(listed(&watcher), [false, false], "while the lock is held");
    fs::remove_file(git_dir.join("index.lock")).unwrap();
    read(&mut watcher, WAIT);
    assert_eq!(listed(&watcher), [true, true]);
}
