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

fn tracked(tree: &str, paths: &[&str]) -> (PathBuf, Vec<String>) {
    (tree.into(), paths.iter().map(|path| path.to_string()).collect())
}

#[test]
fn a_watcher_scans_the_files_a_git_index_lists_below_a_folder_a_gitignore_excludes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "dist/\n");
    for path in ["dist/app.js", "dist/other.js", "dist/sub/deep.js", "repo/dist/x.js"] {
        write(root, path, path);
    }
    git(root, &["init", "-q"]);
    git(root, &["add", "-f", "dist/app.js"]);
    git(&root.join("repo"), &["init", "-q"]);
    git(&root.join("repo"), &["add", "-f", "dist/x.js"]);
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot.rescan_with_rules(root, whole(), &mut rules).unwrap();
    let expected = [".gitignore", "dist", "dist/app.js", "repo", "repo/dist", "repo/dist/x.js"];
    assert_eq!(known(&snapshot), expected);
    let both = [tracked("", &["dist/app.js"]), tracked("repo", &["dist/x.js"])];
    assert_eq!(rescan.tracked, both);

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
        (changes, tracked)
    };
    git(root, &["add", "-f", "dist/sub/deep.js"]);
    let deep = [created("dist/sub", Dir), created("dist/sub/deep.js", File)];
    let now = [tracked("", &["dist/app.js", "dist/sub/deep.js"])];
    assert_eq!(settle(&mut watcher), (deep.to_vec(), now.to_vec()));
    git(root, &["rm", "-q", "--cached", "dist/app.js"]);
    let now = [tracked("", &["dist/sub/deep.js"])];
    assert_eq!(settle(&mut watcher), (Vec::new(), now.to_vec()));
    assert!(!watcher.snapshot().entries.contains_key(Path::new("dist/app.js")));
    assert!(root.join("dist/app.js").exists());
}
