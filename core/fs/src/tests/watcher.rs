// SPDX-License-Identifier: Apache-2.0

const WAIT: Duration = Duration::from_secs(10);

#[test]
fn idle_native_poll_reports_an_unreadable_folder() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let locked = root.join("d/locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("x"), "x").unwrap();
    let snapshot = Snapshot::scan(root).unwrap();
    let mut watcher = NativeWatcher::start(root, snapshot).unwrap();
    let watched = watcher.root.clone();
    watcher.watcher.unwatch(&watched).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let probe = fs::symlink_metadata(locked.join("x"));
    if !blocks_reads("idle_native_poll_reports_an_unreadable_folder", probe) {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let first = watcher.poll(Duration::from_millis(200));
    let second = watcher.poll(Duration::from_millis(200));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        first.unwrap(),
        Poll {
            unreadable: vec![PathBuf::from("d/locked")],
            root: Some(root_id(root)),
            ..Poll::default()
        }
    );
    assert_eq!(second.unwrap(), Poll::default());
}

type Fail = fn(&Path) -> notify::Error;

/// A watcher that registers folders the way notify's inotify backend does: a
/// recursive registration walks the tree and registers one folder at a time,
/// stopping at the first folder that fails, and the folders before it stay
/// registered.
#[derive(Default)]
struct Registrations {
    fail: Vec<(PathBuf, Fail)>,
    watched: BTreeSet<PathBuf>,
}

impl Registrations {
    fn register(&mut self, path: &Path) -> notify::Result<()> {
        if let Some((_, error)) = self.fail.iter().find(|(at, _)| at == path) {
            return Err(error(path));
        }
        self.watched.insert(path.to_path_buf());
        Ok(())
    }

    fn register_tree(&mut self, path: &Path) -> notify::Result<()> {
        self.register(path)?;
        let mut folders: Vec<PathBuf> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect();
        folders.sort();
        folders
            .iter()
            .try_for_each(|folder| self.register_tree(folder))
    }
}

impl notify::Watcher for Registrations {
    fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
        Ok(Self::default())
    }

    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
        match mode {
            RecursiveMode::Recursive => self.register_tree(path),
            RecursiveMode::NonRecursive => self.register(path),
        }
    }

    fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
        Ok(())
    }

    fn kind() -> notify::WatcherKind {
        notify::WatcherKind::NullWatcher
    }
}

#[test]
fn registration_covers_the_tree_around_a_failed_folder() {
    use std::os::unix::fs::PermissionsExt;

    let denied: Fail = |_| notify::Error::io(io::Error::from(io::ErrorKind::PermissionDenied));
    let vanishing: Fail = |path| {
        fs::remove_dir_all(path).unwrap();
        notify::Error::new(notify::ErrorKind::PathNotFound)
    };
    let not_found: Fail = |_| notify::Error::new(notify::ErrorKind::PathNotFound);
    let full: Fail = |_| notify::Error::new(notify::ErrorKind::MaxFilesWatch);
    let unsearchable: Fail = |path| {
        let parent = path.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o000)).unwrap();
        notify::Error::new(notify::ErrorKind::PathNotFound)
    };
    let register = |fail: &[(&str, Fail)]| {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for folder in ["0-locked", "a/gone", "a/still/deep", "b"] {
            fs::create_dir_all(root.join(folder)).unwrap();
        }
        let mut watcher = Registrations {
            fail: fail
                .iter()
                .map(|(name, error)| (root.join(name), *error))
                .collect(),
            ..Registrations::default()
        };
        let result = watch_tree(&mut watcher, &root);
        let _ = fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o755));
        let watched: BTreeSet<String> = watcher
            .watched
            .iter()
            .map(|path| path.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        (result, watched)
    };
    let names = |names: &[&str]| -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    };

    let (result, watched) = register(&[("0-locked", denied), ("a/gone", vanishing)]);
    result.unwrap();
    assert_eq!(watched, names(&["", "a", "a/still", "a/still/deep", "b"]));

    let (result, watched) = register(&[("a/gone", vanishing)]);
    result.unwrap();
    assert_eq!(
        watched,
        names(&["", "0-locked", "a", "a/still", "a/still/deep", "b"])
    );

    for fail in [
        &[("0-locked", denied), ("a/gone", not_found)][..],
        &[("a/gone", not_found)][..],
        &[("0-locked", denied), ("a/still", unsearchable)][..],
    ] {
        let (result, _) = register(fail);
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, notify::ErrorKind::PathNotFound),
            "{err:?}"
        );
    }

    let (result, _) = register(&[("", vanishing)]);
    let err = result.unwrap_err();
    assert!(
        matches!(err.kind, notify::ErrorKind::PathNotFound),
        "{err:?}"
    );

    let (result, _) = register(&[("0-locked", denied), ("b", full)]);
    let err = result.unwrap_err();
    assert!(
        matches!(err.kind, notify::ErrorKind::MaxFilesWatch),
        "{err:?}"
    );
}

#[test]
fn folder_behind_an_unsearchable_parent_is_not_gone() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("a");
    let child = parent.join("child");
    fs::create_dir_all(&child).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = blocks_reads(
        "folder_behind_an_unsearchable_parent_is_not_gone",
        fs::symlink_metadata(&child),
    );
    let unknown = gone(&child);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    if blocked {
        assert!(!unknown);
    }
    assert!(!gone(&child));
    fs::remove_dir(&child).unwrap();
    assert!(gone(&child));
    fs::write(&child, "f").unwrap();
    assert!(gone(&child));
    assert!(gone(&child.join("below")));
}

fn wait_for(watcher: &mut NativeWatcher, expected: &[Change]) -> Vec<Change> {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while !expected.iter().all(|change| seen.contains(change)) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected:?}; saw {seen:?}"
        );
        seen.extend(watcher.poll(Duration::from_millis(500)).unwrap().changes);
    }
    seen
}

fn lose_coverage(watcher: &mut NativeWatcher) {
    let (sender, events) = mpsc::channel();
    watcher.watcher = notify::recommended_watcher(sender.clone()).unwrap();
    watcher
        .watcher
        .watch(&watcher.root, RecursiveMode::NonRecursive)
        .unwrap();
    (watcher.sender, watcher.events) = (sender, events);
}

#[test]
fn rescan_request_restores_lost_watch_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    let mut watcher = NativeWatcher::start(&root, Snapshot::default()).unwrap();
    wait_for(&mut watcher, &[created("sub", EntryKind::Dir)]);
    watcher.poll(Duration::from_millis(300)).unwrap();

    lose_coverage(&mut watcher);
    fs::write(root.join("sub/missed.txt"), "m").unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(
        Poll {
            root: None,
            ..watcher.poll(Duration::from_millis(500)).unwrap()
        },
        Poll::default()
    );

    watcher
        .sender
        .send(Ok(
            notify::Event::new(EventKind::Other).set_flag(Flag::Rescan)
        ))
        .unwrap();
    wait_for(&mut watcher, &[created("sub/missed.txt", EntryKind::File)]);

    fs::write(root.join("sub/later.txt"), "l").unwrap();
    wait_for(&mut watcher, &[created("sub/later.txt", EntryKind::File)]);
}

#[cfg(target_os = "linux")]
#[test]
fn new_folders_are_registered_before_their_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut watcher = NativeWatcher::start(&root, Snapshot::default()).unwrap();
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        Poll {
            root: Some(root_id(&root)),
            ..Poll::default()
        }
    );

    lose_coverage(&mut watcher);
    let early = root.join("sub/early.txt");
    watcher.before_register = Some(Box::new(move || fs::write(&early, "e").unwrap()));
    fs::create_dir(root.join("sub")).unwrap();
    wait_for(
        &mut watcher,
        &[
            created("sub", EntryKind::Dir),
            created("sub/early.txt", EntryKind::File),
        ],
    );

    watcher.before_register = None;
    fs::write(root.join("sub/late.txt"), "l").unwrap();
    wait_for(&mut watcher, &[created("sub/late.txt", EntryKind::File)]);
}

#[cfg(target_os = "linux")]
#[test]
fn folders_included_by_the_registration_rescan_keep_watch_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir(root.join("ig")).unwrap();
    fs::write(root.join(RULES_FILE), "ig\n").unwrap();
    fs::write(root.join("ig/file.txt"), "before").unwrap();
    let rules = Rules::new(Box::new(Lines::default()));
    let mut watcher = NativeWatcher::start_with_rules(&root, Snapshot::default(), rules).unwrap();
    let mut remove = Some(root.join(RULES_FILE));
    watcher.before_register = Some(Box::new(move || {
        if let Some(path) = remove.take() {
            fs::remove_file(path).unwrap();
        }
    }));

    let first = watcher.poll(Duration::ZERO).unwrap();
    watcher.before_register = None;
    assert_eq!(first.rules, [(PathBuf::new(), Some(b"ig\n".to_vec()))]);
    let second = watcher.poll(Duration::ZERO).unwrap();
    assert_eq!(second.rules, [(PathBuf::new(), None)]);
    assert!(second.changes.contains(&created("ig", EntryKind::Dir)));
    assert!(
        second
            .changes
            .contains(&created("ig/file.txt", EntryKind::File))
    );
    assert!(
        watcher
            .poll(Duration::from_millis(150))
            .unwrap()
            .changes
            .is_empty()
    );

    fs::write(root.join("ig/file.txt"), "after the rules were removed").unwrap();
    let expected = Change::Modified {
        path: "ig/file.txt".into(),
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut seen = Vec::new();
    while Instant::now() < deadline && !seen.contains(&expected) {
        seen.extend(watcher.poll(Duration::from_millis(100)).unwrap().changes);
    }
    assert!(
        seen.contains(&expected),
        "the write inside the newly included folder was not reported; saw {seen:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_rename_inside_a_folder_a_poll_moved_comes_in_a_poll_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/file"), "x").unwrap();
    let rules = Rules::new(Box::new(Lines::default()));
    let mut watcher = NativeWatcher::start_with_rules(&root, Snapshot::default(), rules).unwrap();
    while !watcher
        .poll(Duration::from_millis(300))
        .unwrap()
        .changes
        .is_empty()
    {}
    let (from, to) = (root.join("b/file"), root.join("b/moved"));
    let mut once = Some(());
    watcher.before_register = Some(Box::new(move || {
        if once.take().is_some() {
            fs::rename(&from, &to).unwrap();
        }
    }));

    fs::rename(root.join("a"), root.join("b")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut polls = Vec::new();
    while Instant::now() < deadline {
        let poll = watcher.poll(Duration::from_millis(200)).unwrap();
        if !poll.changes.is_empty() {
            polls.push(poll.changes);
        }
    }
    assert_eq!(
        polls,
        [
            vec![Change::Renamed {
                from: "a".into(),
                to: "b".into(),
                kind: EntryKind::Dir,
            }],
            vec![Change::Renamed {
                from: "b/file".into(),
                to: "b/moved".into(),
                kind: EntryKind::File,
            }],
        ]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_poll_stopped_before_it_registers_its_folders_registers_them_on_the_next_poll() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("sub/file.txt"), "before").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let rules = Rules::new(Box::new(Lines::default()));
    let mut watcher = NativeWatcher::start_with_rules(&root, Snapshot::default(), rules)
        .unwrap()
        .stopping_when(move || stopped.load(Ordering::SeqCst));
    let stopping = stop.clone();
    watcher.before_register = Some(Box::new(move || stopping.store(true, Ordering::SeqCst)));

    let first = watcher.poll(Duration::ZERO).unwrap();
    assert_eq!(
        first.changes,
        [
            created("sub", EntryKind::Dir),
            created("sub/file.txt", EntryKind::File)
        ]
    );
    assert_eq!(first.pending, [PathBuf::new()]);
    watcher.before_register = None;
    stop.store(false, Ordering::SeqCst);
    watcher.poll(Duration::from_millis(150)).unwrap();

    fs::write(root.join("sub/file.txt"), "after").unwrap();
    let modified = Change::Modified {
        path: "sub/file.txt".into(),
    };
    wait_for(&mut watcher, &[modified]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_new_folder_that_vanishes_before_its_watch_is_not_a_watch_failure() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut watcher = NativeWatcher::start(&root, Snapshot::default()).unwrap();
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        Poll {
            root: Some(root_id(&root)),
            ..Poll::default()
        }
    );

    let sub = root.join("tmp-build");
    let gone = sub.clone();
    watcher.before_register = Some(Box::new(move || fs::remove_dir(&gone).unwrap()));
    fs::create_dir(&sub).unwrap();
    let seen = wait_for(
        &mut watcher,
        &[
            created("tmp-build", EntryKind::Dir),
            Change::Removed {
                path: "tmp-build".into(),
                kind: EntryKind::Dir,
            },
        ],
    );
    assert_eq!(seen.len(), 2, "{seen:?}");

    watcher.before_register = None;
    assert_eq!(
        Poll {
            root: None,
            ..watcher.poll(Duration::from_millis(300)).unwrap()
        },
        Poll::default()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_new_folder_rescan_that_reaches_another_root_fails_the_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("space");
    fs::create_dir(&root).unwrap();
    let first = root_id(&root);
    let mut watcher = NativeWatcher::start(&root, Snapshot::default()).unwrap();
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        Poll {
            root: Some(first),
            ..Poll::default()
        }
    );

    let (path, moved) = (root.clone(), dir.path().join("moved"));
    watcher.before_register = Some(Box::new(move || {
        fs::rename(&path, &moved).unwrap();
        fs::create_dir_all(path.join("sub")).unwrap();
        fs::write(path.join("sub").join("theirs"), b"replacement").unwrap();
    }));
    fs::create_dir(root.join("sub")).unwrap();
    let poll = watcher.poll(WAIT).unwrap();
    assert_eq!(poll.changes, [created("sub", EntryKind::Dir)]);
    assert_eq!(poll.root, Some(first));
    assert_ne!(root_id(&root), first);
    assert!(matches!(
        watcher.poll(Duration::from_millis(300)),
        Err(Error::RootReplaced)
    ));
    assert!(
        !watcher
            .snapshot()
            .entries
            .contains_key(Path::new("sub/theirs")),
        "the snapshot took entries from the replacement root"
    );
}

#[test]
fn a_rescan_expecting_a_root_born_at_another_time_changes_nothing() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("theirs"), b"replacement").unwrap();
    let found = root_id(&root);
    let Some(born) = found.birth_ns else {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED a_rescan_expecting_a_root_born_at_another_time_changes_nothing: no birth time"
        );
        return;
    };
    let reused = FileId {
        birth_ns: Some(born - 1),
        ..found
    };
    let mut snapshot = Snapshot::default();
    let result = snapshot.rescan_with(&root, [PathBuf::new()], Some(reused), &mut |_| {});
    assert!(matches!(result, Err(Error::RootReplaced)), "{result:?}");
    assert!(snapshot.entries.is_empty());
}
