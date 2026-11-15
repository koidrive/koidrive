// SPDX-License-Identifier: Apache-2.0

use std::fs;

use notify::Watcher as _;
use notify::event::Flag;

use super::*;

const WAIT: Duration = Duration::from_secs(10);

fn blocks_reads<T: std::fmt::Debug>(test: &str, probe: std::io::Result<T>) -> bool {
    use std::io::Write;

    match probe {
        Err(err) => {
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{test}: {err}"
            );
            true
        }
        Ok(found) => {
            assert!(
                rustix::process::geteuid().is_root(),
                "{test}: the permission change did not block reads ({found:?})"
            );
            let _ = writeln!(
                std::io::stderr(),
                "SKIPPED {test}: running as root, so permissions do not block reads"
            );
            false
        }
    }
}

fn tree(root: &Path, files: usize) {
    fs::create_dir_all(root.join("d")).unwrap();
    fs::write(root.join("d/x"), "x").unwrap();
    for i in 0..files {
        fs::write(root.join(format!("f{i}.txt")), "f").unwrap();
    }
}

fn scoped(files: usize) -> Vec<PathBuf> {
    (0..files)
        .map(|i| PathBuf::from(format!("f{i}.txt")))
        .collect()
}

#[test]
fn root_moved_after_opening_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    tree(&root, 200);
    for paths in [vec![PathBuf::new()], scoped(200)] {
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        let moved = dir.path().join("moved");
        let result = snapshot.rescan_with(&root, paths, None, &mut |cut| {
            if let Cut::RootOpened = cut {
                fs::rename(&root, &moved).unwrap();
            }
        });
        assert!(
            matches!(&result, Err(Error::Io(err)) if err.kind() == io::ErrorKind::NotFound),
            "{result:?}"
        );
        assert_eq!(snapshot, before);
        fs::rename(&moved, &root).unwrap();
    }
}

#[test]
fn root_removed_after_opening_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    for paths in [vec![PathBuf::new()], scoped(200)] {
        tree(&root, 200);
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        let result = snapshot.rescan_with(&root, paths, None, &mut |cut| {
            if let Cut::RootOpened = cut {
                fs::remove_dir_all(&root).unwrap();
            }
        });
        assert!(
            matches!(&result, Err(Error::Io(err)) if err.kind() == io::ErrorKind::NotFound),
            "{result:?}"
        );
        assert_eq!(snapshot, before);
    }
}

#[test]
fn root_replaced_after_opening_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    tree(&root, 20);
    let mut snapshot = Snapshot::scan(&root).unwrap();
    let before = snapshot.clone();
    let result = snapshot.rescan_with(&root, [PathBuf::new()], None, &mut |cut| {
        if let Cut::RootOpened = cut {
            fs::rename(&root, dir.path().join("moved")).unwrap();
            fs::create_dir(&root).unwrap();
        }
    });
    assert!(matches!(result, Err(Error::RootReplaced)), "{result:?}");
    assert_eq!(snapshot, before);
}

#[test]
fn folder_replaced_during_traversal_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("x"), "outside").unwrap();
    for replacement in ["symlink", "folder"] {
        tree(&root, 0);
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        let result = snapshot.rescan_with(&root, [PathBuf::new()], None, &mut |cut| {
            if let Cut::OpeningDir(path) = cut
                && path == Path::new("d")
            {
                fs::rename(root.join("d"), root.join("d-old")).unwrap();
                match replacement {
                    "symlink" => std::os::unix::fs::symlink(&outside, root.join("d")).unwrap(),
                    _ => fs::create_dir(root.join("d")).unwrap(),
                }
            }
        });
        assert!(
            matches!(result, Err(Error::Changed)),
            "{replacement}: {result:?}"
        );
        assert_eq!(snapshot, before, "{replacement}");
        fs::remove_dir_all(&root).unwrap();
    }
}

#[test]
fn ancestor_replaced_after_opening_publishes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("z-victim"), "outside").unwrap();
    let scopes = [
        vec![PathBuf::new()],
        vec![PathBuf::from("d/a-trigger"), PathBuf::from("d/z-victim")],
    ];
    for replacement in ["unreadable", "symlink"] {
        for paths in scopes.clone() {
            fs::create_dir_all(root.join("d/a-trigger")).unwrap();
            fs::write(root.join("d/z-victim"), "user content").unwrap();
            let detached = dir.path().join("detached-d");
            let mut snapshot = Snapshot::scan(&root).unwrap();
            let before = snapshot.clone();

            let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
                if let Cut::OpeningDir(path) = cut
                    && path == Path::new("d/a-trigger")
                {
                    fs::rename(root.join("d"), &detached).unwrap();
                    match replacement {
                        "unreadable" => {
                            fs::create_dir(root.join("d")).unwrap();
                            fs::write(root.join("d/z-victim"), "new user content").unwrap();
                            fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o000))
                                .unwrap();
                        }
                        _ => std::os::unix::fs::symlink(&outside, root.join("d")).unwrap(),
                    }
                    fs::remove_file(detached.join("z-victim")).unwrap();
                }
            });

            assert!(
                matches!(result, Err(Error::Changed)),
                "{replacement} {paths:?}: {result:?}"
            );
            assert_eq!(snapshot, before, "{replacement} {paths:?}");
            if replacement == "unreadable" {
                fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
            }
            fs::remove_dir_all(&root).unwrap();
            fs::remove_dir_all(&detached).unwrap();
        }
    }
}

#[test]
fn ancestor_swapped_out_and_back_publishes_no_removal() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let detached = dir.path().join("detached-d");
    let replacement = dir.path().join("replacement-d");
    let scopes = [
        vec![PathBuf::new()],
        vec![
            PathBuf::from("d/a-trigger"),
            PathBuf::from("d/m-victim"),
            PathBuf::from("d/z-trigger"),
        ],
    ];
    for paths in scopes {
        fs::create_dir_all(root.join("d/a-trigger")).unwrap();
        fs::create_dir_all(root.join("d/z-trigger")).unwrap();
        fs::write(root.join("d/m-victim"), "user content").unwrap();
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();

        let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
            let Cut::OpeningDir(path) = cut else { return };
            if path == Path::new("d/a-trigger") {
                fs::rename(root.join("d"), &detached).unwrap();
                fs::create_dir(root.join("d")).unwrap();
                fs::write(root.join("d/m-victim"), "user content").unwrap();
                fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o000)).unwrap();
                fs::remove_file(detached.join("m-victim")).unwrap();
            } else if path == Path::new("d/z-trigger") {
                fs::write(detached.join("m-victim"), "user content").unwrap();
                fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
                fs::rename(root.join("d"), &replacement).unwrap();
                fs::rename(&detached, root.join("d")).unwrap();
            }
        });

        let rescan = result.unwrap_or_else(|err| panic!("{paths:?}: {err:?}"));
        assert_eq!(rescan.changes, [], "{paths:?}");
        assert_eq!(rescan.unsettled, [PathBuf::from("d/m-victim")], "{paths:?}");
        assert_eq!(snapshot, before, "{paths:?}");
        assert!(root.join("d/m-victim").exists());
        fs::remove_dir_all(&replacement).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}

#[test]
fn parent_swapped_during_confirmation_publishes_no_removal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let detached = dir.path().join("detached-d");
    for paths in [vec![PathBuf::new()], vec![PathBuf::from("d/sub/victim")]] {
        fs::create_dir_all(root.join("d/sub")).unwrap();
        fs::write(root.join("d/sub/victim"), "user content").unwrap();
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        fs::remove_file(root.join("d/sub/victim")).unwrap();

        let mut swapped = false;
        let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
            let at_d = matches!(cut, Cut::Confirming(path) if path == Path::new("d"));
            if swapped || !at_d {
                return;
            }
            swapped = true;
            fs::rename(root.join("d"), &detached).unwrap();
            fs::create_dir_all(root.join("d/sub")).unwrap();
            fs::write(root.join("d/sub/victim"), "user content").unwrap();
        });

        assert!(swapped, "{paths:?}");
        let rescan = result.unwrap_or_else(|err| panic!("{paths:?}: {err:?}"));
        assert_eq!(rescan.changes, [], "{paths:?}");
        assert_eq!(
            rescan.unsettled,
            [PathBuf::from("d/sub/victim")],
            "{paths:?}"
        );
        assert_eq!(snapshot, before, "{paths:?}");
        assert!(root.join("d/sub/victim").exists());
        fs::remove_dir_all(&detached).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}

#[test]
fn sibling_recreated_after_its_parent_was_listed_publishes_no_removal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let files = ["d/a-first", "d/m/y", "d/z-victim"];
    let scopes = [
        vec![PathBuf::new()],
        files.iter().map(PathBuf::from).collect(),
    ];
    for paths in scopes {
        fs::create_dir_all(root.join("d/m")).unwrap();
        for file in files {
            fs::write(root.join(file), "user content").unwrap();
        }
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let mut expected = snapshot.clone();
        expected.entries.remove(Path::new("d/m/y"));
        for file in files {
            fs::remove_file(root.join(file)).unwrap();
        }

        let mut recreated = false;
        let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
            let at_m = matches!(cut, Cut::Confirming(path) if path == Path::new("d/m"));
            if recreated || !at_m {
                return;
            }
            recreated = true;
            fs::write(root.join("d/z-victim"), "recreated by the user").unwrap();
        });

        assert!(recreated, "{paths:?}");
        let rescan = result.unwrap_or_else(|err| panic!("{paths:?}: {err:?}"));
        assert_eq!(
            rescan.changes,
            [Change::Removed {
                path: PathBuf::from("d/m/y"),
                kind: EntryKind::File
            }],
            "{paths:?}"
        );
        assert_eq!(
            rescan.unsettled,
            [PathBuf::from("d/a-first"), PathBuf::from("d/z-victim")],
            "{paths:?}"
        );
        assert_eq!(snapshot, expected, "{paths:?}");

        let settled = snapshot.rescan(&root, rescan.unsettled).unwrap();
        assert_eq!(
            settled,
            Rescan {
                changes: vec![
                    Change::Removed {
                        path: PathBuf::from("d/a-first"),
                        kind: EntryKind::File
                    },
                    Change::Modified {
                        path: PathBuf::from("d/z-victim")
                    },
                ],
                unsettled: vec![],
                unreadable: vec![],
                root: root_id(&root),
                rules: vec![],
                read: vec![],
            },
            "{paths:?}"
        );
        assert_eq!(snapshot, Snapshot::scan(&root).unwrap(), "{paths:?}");
        fs::remove_dir_all(&root).unwrap();
    }
}

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

fn created(path: &str, kind: EntryKind) -> Change {
    Change::Created {
        path: path.into(),
        kind,
    }
}

fn root_id(root: &Path) -> FileId {
    stat_fd(open_root(root).unwrap().as_fd()).unwrap().id
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

#[test]
fn utc_dates_follow_the_calendar() {
    let date = |secs: i64| {
        let time = if secs < 0 {
            std::time::UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
        } else {
            std::time::UNIX_EPOCH + Duration::from_secs(secs as u64)
        };
        recovered::utc_date(time)
    };
    assert_eq!(date(0), "1970-01-01");
    assert_eq!(date(-1), "1969-12-31");
    assert_eq!(date(951_782_400), "2000-02-29");
    assert_eq!(date(951_868_800), "2000-03-01");
    assert_eq!(date(4_107_456_000), "2100-02-28");
    assert_eq!(date(4_107_542_400), "2100-03-01");
}

#[test]
fn a_recovered_name_is_not_given_in_a_folder_moved_out_of_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir(&outside).unwrap();
    let space = Space::bind(open_root(&root).unwrap(), &root).unwrap();
    let held = space.private(&["held"]).unwrap();
    fs::write(root.join(PRIVATE_DIR).join("held/e1"), "kept").unwrap();
    let id = Snapshot::scan(&root).unwrap().entries[Path::new("docs")].id;
    let docs = space
        .folder(
            &Base::Root,
            &[Dir {
                name: "docs".into(),
                id,
            }],
        )
        .unwrap();
    fs::rename(root.join("docs"), outside.join("docs")).unwrap();

    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let original = std::ffi::OsStr::new("report.txt");
    let path = space
        .move_to_recovered_name(
            &held,
            std::ffi::OsStr::new("e1"),
            &docs,
            original,
            false,
            now,
        )
        .unwrap();

    let name = recovered::recovered_name(original, false, "2023-11-14", 1);
    assert_eq!(path, PathBuf::from(&name));
    assert_eq!(fs::read(root.join(&name)).unwrap(), b"kept");
    assert_eq!(fs::read_dir(outside.join("docs")).unwrap().count(), 0);
}

#[test]
fn nothing_is_recovered_from_a_folder_moved_out_of_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    let space = Space::bind(open_root(&root).unwrap(), &root).unwrap();
    let held = space.private(&["held"]).unwrap();
    fs::write(root.join(PRIVATE_DIR).join("held/e1"), "kept").unwrap();
    fs::rename(root.join(PRIVATE_DIR).join("held"), outside.join("held")).unwrap();

    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let to = space.root_folder().unwrap();
    let original = std::ffi::OsStr::new("report.txt");
    space
        .move_to_recovered_name(&held, std::ffi::OsStr::new("e1"), &to, original, false, now)
        .unwrap_err();

    assert_eq!(fs::read(outside.join("held/e1")).unwrap(), b"kept");
    let names: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, [PRIVATE_DIR]);
}

#[test]
fn every_parked_entry_is_moved_to_the_root_under_a_recovered_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let space = Space::bind(open_root(&root).unwrap(), &root).unwrap();
    space.private(&[PARKED_DIR]).unwrap();
    let parked = root.join(PRIVATE_DIR).join(PARKED_DIR);
    fs::create_dir(parked.join("n1")).unwrap();
    fs::write(parked.join("n1/inside.txt"), "kept").unwrap();
    fs::write(parked.join("n2"), "also kept").unwrap();

    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let name = |parked: &std::ffi::OsStr, _| match parked.to_str() {
        Some("n1") => "docs".into(),
        _ => parked.to_owned(),
    };
    let mut moved = space.bring_back_all(name, now).unwrap();
    moved.sort();

    let docs = recovered::recovered_name("docs".as_ref(), true, "2023-11-14", 1);
    let n2 = recovered::recovered_name("n2".as_ref(), false, "2023-11-14", 1);
    assert_eq!(moved, [PathBuf::from(&docs), PathBuf::from(&n2)]);
    assert_eq!(
        fs::read(root.join(&docs).join("inside.txt")).unwrap(),
        b"kept"
    );
    assert_eq!(fs::read(root.join(&n2)).unwrap(), b"also kept");
    assert_eq!(fs::read_dir(&parked).unwrap().count(), 0);
}

#[test]
fn an_entry_an_effect_left_in_quarantine_is_moved_to_the_root_under_its_recovered_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let space = Space::bind(open_root(&root).unwrap(), &root).unwrap();
    space.private(&[QUARANTINE_DIR]).unwrap();
    let quarantine = root.join(PRIVATE_DIR).join(QUARANTINE_DIR);
    fs::write(quarantine.join("effect-7"), "displaced").unwrap();
    let ino =
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(quarantine.join("effect-7")).unwrap());

    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let name = |held: &std::ffi::OsStr, id: FileId| match id.ino == u128::from(ino) {
        true => "notes.txt".into(),
        false => held.to_owned(),
    };
    let moved = space.bring_back_all(name, now).unwrap();

    let notes = recovered::recovered_name("notes.txt".as_ref(), false, "2023-11-14", 1);
    assert_eq!(moved, [PathBuf::from(&notes)]);
    assert_eq!(fs::read(root.join(&notes)).unwrap(), b"displaced");
    assert_eq!(fs::read_dir(&quarantine).unwrap().count(), 0);
}

#[test]
fn the_entries_a_store_rollback_brings_back_are_remembered() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let space = Space::bind(open_root(&root).unwrap(), &root).unwrap();
    space.private(&[PARKED_DIR]).unwrap();
    space.private(&[QUARANTINE_DIR]).unwrap();
    let private = root.join(PRIVATE_DIR);
    let folder = private.join(PARKED_DIR).join("n1");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("inside.txt"), "kept").unwrap();
    fs::set_permissions(folder.join("inside.txt"), fs::Permissions::from_mode(0o444)).unwrap();
    let file = private.join(QUARANTINE_DIR).join("effect-7");
    fs::write(&file, "displaced").unwrap();
    let id = |path: &Path| {
        let meta = fs::symlink_metadata(path).unwrap();
        (meta.dev(), u128::from(meta.ino()))
    };
    let ids = |space: &Space| {
        let BroughtBack::Entries(ids) = space.brought_back().unwrap() else {
            panic!("the record is unreadable");
        };
        let mut ids: Vec<_> = (ids.into_iter()).map(|id| (id.dev, id.ino)).collect();
        ids.sort();
        ids
    };
    let mut expected = vec![id(&folder), id(&file)];
    expected.sort();
    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

    assert_eq!(
        space
            .bring_back_all(|held, _| held.to_owned(), now)
            .unwrap()
            .len(),
        2
    );
    let space = Space::bind(open_root(&root).unwrap(), &root).unwrap();
    assert_eq!(ids(&space), expected);

    let later = private.join(PARKED_DIR).join("n3");
    fs::write(&later, "later").unwrap();
    expected.push(id(&later));
    expected.sort();
    assert_eq!(
        space
            .bring_back_all(|held, _| held.to_owned(), now)
            .unwrap()
            .len(),
        1
    );
    assert!(
        space
            .bring_back_all(|held, _| held.to_owned(), now)
            .unwrap()
            .is_empty()
    );
    assert_eq!(ids(&space), expected);
    space.repair_brought_back().unwrap();
    assert_eq!(ids(&space), expected);
}

#[test]
fn a_partial_record_of_brought_back_entries_is_repaired_to_every_root_entry() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let space = Space::bind(open_root(&root).unwrap(), &root).unwrap();
    space.private(&[PARKED_DIR]).unwrap();
    let private = root.join(PRIVATE_DIR);
    fs::write(private.join("brought-back.json"), "[[1,2,nu").unwrap();
    fs::write(private.join(PARKED_DIR).join("n1"), "parked").unwrap();
    fs::create_dir(root.join("docs")).unwrap();
    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

    assert_eq!(space.brought_back().unwrap(), BroughtBack::Unreadable);
    let moved = space
        .bring_back_all(|held, _| held.to_owned(), now)
        .unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(space.brought_back().unwrap(), BroughtBack::Unreadable);
    assert!(BroughtBack::Unreadable.holds(FileId {
        dev: 1,
        ino: 2,
        birth_ns: None
    }));

    space.repair_brought_back().unwrap();
    let id = |path: PathBuf| {
        let meta = fs::symlink_metadata(path).unwrap();
        (meta.dev(), u128::from(meta.ino()))
    };
    let expected: std::collections::BTreeSet<_> =
        [id(root.join(&moved[0])), id(root.join("docs"))].into();
    let BroughtBack::Entries(repaired) = space.brought_back().unwrap() else {
        panic!("the record is still unreadable");
    };
    let repaired: std::collections::BTreeSet<_> =
        repaired.into_iter().map(|id| (id.dev, id.ino)).collect();
    assert_eq!(repaired, expected);
}

#[test]
fn an_unreadable_entry_is_still_brought_back() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let space = Space::bind(open_root(&root).unwrap(), &root).unwrap();
    space.private(&[PARKED_DIR]).unwrap();
    space.private(&[QUARANTINE_DIR]).unwrap();
    let held = root.join(PRIVATE_DIR).join(PARKED_DIR).join("n1");
    fs::write(&held, "locked").unwrap();
    fs::set_permissions(&held, fs::Permissions::from_mode(0o000)).unwrap();
    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

    let moved = space
        .bring_back_all(|held, _| held.to_owned(), now)
        .unwrap();

    assert_eq!(moved.len(), 1);
    let brought = fs::symlink_metadata(root.join(&moved[0])).unwrap();
    assert_eq!(brought.permissions().mode() & 0o777, 0o000);
}

#[derive(Clone, Default)]
struct Lines(std::sync::Arc<std::sync::Mutex<BTreeMap<PathBuf, Vec<u8>>>>);

impl Ignore for Lines {
    fn set_rules(&mut self, dir: &Path, rules: Option<&[u8]>) {
        let mut given = self.0.lock().unwrap();
        match rules {
            Some(rules) => given.insert(dir.to_path_buf(), rules.to_vec()),
            None => given.remove(dir),
        };
    }

    fn status(&self, path: &Path, _: EntryKind) -> Ignored {
        let given = self.0.lock().unwrap();
        let mut dir = PathBuf::new();
        for name in path {
            let left_out = given.get(&dir).is_some_and(|rules| {
                rules
                    .split(|byte| *byte == b'\n')
                    .any(|line| line == name.as_bytes())
            });
            if left_out {
                return Ignored::Rule;
            }
            dir.push(name);
        }
        Ignored::No
    }

    fn key(&self, name: &OsStr) -> OsString {
        name.to_os_string()
    }
}

#[test]
fn a_failed_scan_leaves_the_rules_as_they_were() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    for (path, body) in [
        (".koiignore", "ig\n"),
        ("a/.koiignore", "long-name\n"),
        ("a/long-name", "x"),
        ("a/y", "y"),
        ("ig/f", "f"),
        ("m/f", "f"),
        ("z/f", "f"),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let matcher = Lines::default();
    let mut rules = Rules::new(Box::new(matcher.clone()));
    let mut snapshot = Snapshot::scan_with_rules(&root, &mut rules).unwrap();
    let before = snapshot.clone();
    let given = matcher.0.lock().unwrap().clone();
    assert_eq!(given.len(), 2);

    fs::write(root.join(".koiignore"), "z\n").unwrap();
    fs::write(root.join("a/.koiignore"), "y\n").unwrap();
    let result = snapshot.rescan_under(
        &root,
        [PathBuf::new()],
        None,
        Some(&mut rules),
        &|| false,
        &mut |cut| {
            if let Cut::OpeningDir(path) = cut
                && path == Path::new("m")
            {
                assert_ne!(*matcher.0.lock().unwrap(), given);
                fs::rename(root.join("m"), root.join("m-old")).unwrap();
                fs::create_dir(root.join("m")).unwrap();
            }
        },
    );
    assert!(matches!(result, Err(Error::Changed)), "{result:?}");
    assert_eq!(snapshot, before);
    assert_eq!(*matcher.0.lock().unwrap(), given);
    assert_eq!(rules.status(Path::new("ig"), EntryKind::Dir), Ignored::Rule);
    assert_eq!(rules.status(Path::new("z"), EntryKind::Dir), Ignored::No);

    fs::remove_dir(root.join("m")).unwrap();
    fs::rename(root.join("m-old"), root.join("m")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(&root, [PathBuf::new()], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.rules,
        [
            (PathBuf::new(), Some(b"z\n".to_vec())),
            (PathBuf::from("a"), Some(b"y\n".to_vec())),
        ]
    );
    assert_eq!(
        rescan.changes,
        [
            created("a/long-name", EntryKind::File),
            created("ig", EntryKind::Dir),
            created("ig/f", EntryKind::File),
            Change::Modified {
                path: ".koiignore".into()
            },
            Change::Modified {
                path: "a/.koiignore".into()
            },
        ]
    );
}

#[test]
fn a_crash_while_trashing_brings_nothing_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("report.txt"), "synced").unwrap();
    let space = Space::bind(fs::File::open(root).unwrap().into(), root).unwrap();
    let location = Location::At {
        base: Base::Root,
        dirs: Vec::new(),
        name: "report.txt".into(),
    };
    let (folder, name) = space.parent(&location).unwrap();
    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = space.trash_with(&folder, name, "e1", name, None, now, &mut || {
            panic!("crash")
        });
    }));
    assert!(crash.is_err());

    let mut recovered = Vec::new();
    for later in [now, now + TRASH_RETENTION, now + 2 * TRASH_RETENTION] {
        recovered.extend(
            space
                .reclaim_trash(later, u64::MAX, &mut |_, _| Ok(true))
                .unwrap()
                .recovered,
        );
    }
    assert_eq!(recovered, Vec::<OsString>::new());
}

#[test]
fn a_search_looks_at_a_bounded_number_of_entries_per_rescan_under_deep_and_wide_left_out_folders() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let deep: PathBuf = ["ig", "wide"].into_iter().chain(["d"; 100]).collect();
    fs::create_dir_all(root.join(&deep)).unwrap();
    fs::write(root.join(deep.join("f")), "f").unwrap();
    for i in 0..1000 {
        fs::write(root.join(format!("ig/wide/{i}")), "w").unwrap();
        fs::write(root.join(deep.parent().unwrap().join(format!("{i}"))), "w").unwrap();
    }
    fs::write(root.join(".koiignore"), "ig\n").unwrap();
    fs::write(root.join("f.txt"), "f").unwrap();
    let budget = 50;
    let mut rules = Rules::new(Box::new(Lines::default())).with_search_budget(budget);
    let mut snapshot = Snapshot::scan_with_rules(&root, &mut rules).unwrap();
    fs::remove_file(root.join("f.txt")).unwrap();

    let mut paths = vec![PathBuf::new()];
    let mut most = 0;
    for _ in 0..1000 {
        let mut looked = 0;
        let rescan = snapshot
            .rescan_under(
                &root,
                paths,
                None,
                Some(&mut rules),
                &|| false,
                &mut |cut| match cut {
                    Cut::Searching(path) | Cut::OpeningDir(path) if path.starts_with("ig") => {
                        looked += 1;
                    }
                    Cut::Searching(path) => panic!("{path:?}"),
                    Cut::Confirming(_) => looked += 1,
                    _ => {}
                },
            )
            .unwrap();
        most = most.max(looked);
        if !rescan.changes.is_empty() {
            assert_eq!(
                rescan.changes,
                [Change::Removed {
                    path: PathBuf::from("f.txt"),
                    kind: EntryKind::File
                }]
            );
            assert!(most <= budget + 1, "{most} entries and opens in one rescan");
            return;
        }
        paths = rescan.unsettled;
    }
    panic!("the search never ended");
}

#[test]
fn a_left_out_entry_back_while_its_removal_is_confirmed_stays_left_out_until_included() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("sub/hidden"), "known").unwrap();
    let mut rules = Rules::new(Box::new(Lines::default()));
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::write(root.join("sub").join(RULES_FILE), "hidden\n").unwrap();
    snapshot
        .rescan_with_rules(root, [PathBuf::new()], &mut rules)
        .unwrap();
    assert!(snapshot.left_out.contains_key(Path::new("sub/hidden")));
    fs::remove_file(root.join("sub/hidden")).unwrap();
    let mut restored = false;
    let rescan = snapshot
        .rescan_under(
            root,
            [PathBuf::new()],
            None,
            Some(&mut rules),
            &|| false,
            &mut |cut| {
                if !restored && matches!(cut, Cut::Confirming(path) if path == Path::new("sub")) {
                    fs::write(root.join("sub/hidden"), "returned during confirmation").unwrap();
                    restored = true;
                }
            },
        )
        .unwrap();
    assert!(restored);
    assert_eq!(rescan.unsettled, [PathBuf::from("sub/hidden")]);
    assert!(!snapshot.entries.contains_key(Path::new("sub/hidden")));
    assert!(snapshot.left_out.contains_key(Path::new("sub/hidden")));
    fs::write(root.join("sub").join(RULES_FILE), "").unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::new()], &mut rules)
        .unwrap();
    assert!(
        rescan
            .changes
            .contains(&created("sub/hidden", EntryKind::File)),
        "{rescan:?}"
    );
}

#[test]
fn a_folder_swapped_for_one_holding_its_entries_is_reidentified() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    fs::create_dir_all(root.join("photos")).unwrap();
    fs::write(root.join("photos/one.jpeg"), "one").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    let before = snapshot.entries[Path::new("photos")].id;

    fs::create_dir(root.join("photos.new")).unwrap();
    fs::rename(
        root.join("photos/one.jpeg"),
        root.join("photos.new/one.jpeg"),
    )
    .unwrap();
    fs::remove_dir(root.join("photos")).unwrap();
    fs::rename(root.join("photos.new"), root.join("photos")).unwrap();
    let rescan = snapshot.rescan(&root, [PathBuf::new()]).unwrap();

    let after = snapshot.entries[Path::new("photos")].id;
    assert_ne!(after, before);
    assert_eq!(
        rescan.changes,
        [Change::Reidentified {
            path: PathBuf::from("photos")
        }]
    );
}

#[test]
fn a_folder_swapped_for_one_holding_its_entries_is_reidentified_by_a_rescan_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    fs::create_dir_all(root.join("photos")).unwrap();
    fs::write(root.join("photos/one.jpeg"), "one").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();

    fs::create_dir(root.join("photos.new")).unwrap();
    fs::rename(
        root.join("photos/one.jpeg"),
        root.join("photos.new/one.jpeg"),
    )
    .unwrap();
    fs::remove_dir(root.join("photos")).unwrap();
    fs::rename(root.join("photos.new"), root.join("photos")).unwrap();
    let rescan = snapshot
        .rescan(
            &root,
            [PathBuf::from("photos"), PathBuf::from("photos.new")],
        )
        .unwrap();

    assert_eq!(
        rescan.changes,
        [Change::Reidentified {
            path: PathBuf::from("photos")
        }]
    );
}
