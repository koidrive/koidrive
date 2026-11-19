// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use koidrive_fs::{
    Change, Ctime, Entry, EntryKind, Error, FileId, Known, NativeWatcher, Poll, Rescan,
    RescanWatcher, Snapshot, Space, Unread, Watcher,
};

const WAIT: Duration = Duration::from_secs(10);

fn created(path: &str, kind: EntryKind) -> Change {
    Change::Created {
        path: path.into(),
        kind,
    }
}

fn modified(path: &str) -> Change {
    Change::Modified { path: path.into() }
}

fn reidentified(path: &str) -> Change {
    Change::Reidentified { path: path.into() }
}

fn renamed(from: &str, to: &str, kind: EntryKind) -> Change {
    Change::Renamed {
        from: from.into(),
        to: to.into(),
        kind,
    }
}

fn removed(path: &str, kind: EntryKind) -> Change {
    Change::Removed {
        path: path.into(),
        kind,
    }
}

use EntryKind::{Dir, File};

/// The identity of the folder at `root`.
fn root_id(root: &Path) -> FileId {
    Space::bind(fs::File::open(root).unwrap().into(), root)
        .unwrap()
        .id()
}

/// What a rescan of `root` that finds nothing returns.
fn nothing(root: &Path) -> Rescan {
    Rescan {
        changes: vec![],
        unsettled: vec![],
        unreadable: vec![],
        root: root_id(root),
        rules: vec![],
        read: vec![],
        tracked: vec![],
        settled: vec![],
    }
}

/// What a poll that scans `root` and finds nothing returns.
fn quiet(root: &Path) -> Poll {
    Poll {
        root: Some(root_id(root)),
        ..Poll::default()
    }
}

/// Whether the permission change a test just made blocks reads, given the read
/// `probe` that should now fail. Only root reads through it; the test is then
/// skipped, with a line written straight to stderr so that it shows up even
/// when test output is captured. Anyone else fails the test.
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

/// Builds a tree, snapshots it, then changes it with no watcher running.
fn tree_changed_offline() -> (tempfile::TempDir, Snapshot, Vec<Change>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (path, body) in [
        ("keep.txt", "same"),
        ("edit.txt", "old"),
        ("move-me.txt", "m"),
        ("gone.txt", "g"),
        ("docs/a.txt", "a"),
        ("docs/deep/b.txt", "b"),
        ("trash/x.txt", "x"),
        ("trash/sub/y.txt", "y"),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let previous = Snapshot::scan(root).unwrap();

    fs::write(root.join("new.txt"), "n").unwrap();
    fs::create_dir(root.join("newdir")).unwrap();
    fs::write(root.join("newdir/inner.txt"), "i").unwrap();
    fs::write(root.join("edit.txt"), "changed content").unwrap();
    fs::rename(root.join("move-me.txt"), root.join("moved.txt")).unwrap();
    fs::rename(root.join("docs"), root.join("papers")).unwrap();
    fs::remove_file(root.join("gone.txt")).unwrap();
    fs::remove_dir_all(root.join("trash")).unwrap();

    let expected = vec![
        removed("gone.txt", File),
        removed("trash", Dir),
        renamed("docs", "papers", Dir),
        renamed("move-me.txt", "moved.txt", File),
        created("new.txt", File),
        created("newdir", Dir),
        created("newdir/inner.txt", File),
        modified("edit.txt"),
    ];
    (dir, previous, expected)
}

#[test]
fn rescan_watcher_catches_changes_made_while_nothing_watched() {
    let (dir, previous, expected) = tree_changed_offline();
    let mut watcher = RescanWatcher::new(dir.path(), previous, Duration::from_millis(20)).unwrap();
    assert_eq!(watcher.poll(WAIT).unwrap().changes, expected);
    assert_eq!(
        watcher.poll(Duration::from_millis(100)).unwrap(),
        quiet(dir.path())
    );
    assert_eq!(watcher.snapshot(), &Snapshot::scan(dir.path()).unwrap());
}

/// A stop flag that holds once it has been checked `checks` times. Storing more checks in the returned count lifts
/// it again.
fn stop_after(checks: usize) -> (Arc<AtomicUsize>, impl Fn() -> bool + Send + 'static) {
    let left = Arc::new(AtomicUsize::new(checks));
    let checked = left.clone();
    let stopped = move || {
        (checked.try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
            left.checked_sub(1)
        }))
        .is_err()
    };
    (left, stopped)
}

/// Ten folders of ten files each.
fn wide_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for folder in 0..10 {
        let path = dir.path().join(format!("f{folder}"));
        fs::create_dir(&path).unwrap();
        for file in 0..10 {
            fs::write(path.join(format!("{file}.txt")), "x").unwrap();
        }
    }
    dir
}

/// The first poll of `watcher`, stopped ten entries into its walk of `root`, fails and leaves the snapshot empty.
/// Once the stop is lifted, the next poll reports the whole tree.
fn stopped_then_resumed(watcher: &mut dyn Watcher, left: &AtomicUsize, root: &Path) {
    assert!(matches!(watcher.poll(WAIT), Err(Error::Stopped)));
    assert_eq!(watcher.snapshot(), &Snapshot::default());
    left.store(usize::MAX, Ordering::SeqCst);
    assert_eq!(watcher.poll(WAIT).unwrap().changes.len(), 110);
    assert_eq!(watcher.snapshot(), &Snapshot::scan(root).unwrap());
}

#[test]
fn a_rescan_watcher_stopped_during_its_walk_keeps_its_snapshot() {
    let dir = wide_tree();
    let (left, stopped) = stop_after(10);
    let watcher = RescanWatcher::new(dir.path(), Snapshot::default(), Duration::from_millis(20));
    let mut watcher = watcher.unwrap().stopping_when(stopped);
    stopped_then_resumed(&mut watcher, &left, dir.path());
}

#[test]
fn a_native_watcher_stopped_during_its_walk_keeps_its_snapshot_and_its_pending_scope() {
    let dir = wide_tree();
    let (left, stopped) = stop_after(10);
    let watcher = NativeWatcher::start(dir.path(), Snapshot::default());
    let mut watcher = watcher.unwrap().stopping_when(stopped);
    stopped_then_resumed(&mut watcher, &left, dir.path());
}

#[test]
fn native_watcher_catches_changes_made_while_nothing_watched() {
    let (dir, previous, expected) = tree_changed_offline();
    let mut watcher = NativeWatcher::start(dir.path(), previous).unwrap();
    assert_eq!(watcher.poll(WAIT).unwrap().changes, expected);
    let poll = watcher.poll(Duration::from_millis(300)).unwrap();
    assert_eq!(Poll { root: None, ..poll }, Poll::default());
    assert_eq!(watcher.snapshot(), &Snapshot::scan(dir.path()).unwrap());
}

/// Polls until every expected change arrived, then a little longer for
/// stragglers. A created file may also show up as modified when its write
/// lands in a later batch than its creation; nothing else may appear.
fn expect(watcher: &mut dyn Watcher, expected: &[Change]) {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while !expected.iter().all(|change| seen.contains(change)) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected:?}; saw {seen:?}"
        );
        seen.extend(watcher.poll(Duration::from_millis(500)).unwrap().changes);
    }
    seen.extend(watcher.poll(Duration::from_millis(300)).unwrap().changes);
    let unexpected: Vec<_> = seen
        .iter()
        .filter(|change| !expected.contains(change))
        .filter(|change| match change {
            Change::Modified { path } => !expected.contains(&Change::Created {
                path: path.clone(),
                kind: File,
            }),
            _ => true,
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected {unexpected:?} while waiting for {expected:?}"
    );
}

fn live_changes(watcher: &mut dyn Watcher, root: &Path) {
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        quiet(root)
    );

    fs::write(root.join("a.txt"), "one").unwrap();
    expect(watcher, &[created("a.txt", File)]);

    fs::write(root.join("a.txt"), "one two").unwrap();
    expect(watcher, &[modified("a.txt")]);

    fs::set_permissions(root.join("a.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    expect(watcher, &[modified("a.txt")]);

    fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
    expect(watcher, &[renamed("a.txt", "b.txt", File)]);

    fs::remove_file(root.join("b.txt")).unwrap();
    expect(watcher, &[removed("b.txt", File)]);

    fs::create_dir(root.join("dir")).unwrap();
    expect(watcher, &[created("dir", Dir)]);

    fs::write(root.join("dir/inner.txt"), "inner").unwrap();
    expect(watcher, &[created("dir/inner.txt", File)]);

    fs::create_dir_all(root.join("dir/sub/leaf")).unwrap();
    expect(
        watcher,
        &[created("dir/sub", Dir), created("dir/sub/leaf", Dir)],
    );

    fs::rename(root.join("dir"), root.join("dir2")).unwrap();
    expect(watcher, &[renamed("dir", "dir2", Dir)]);

    fs::rename(root.join("dir2/inner.txt"), root.join("dir2/sub/inner.txt")).unwrap();
    expect(
        watcher,
        &[renamed("dir2/inner.txt", "dir2/sub/inner.txt", File)],
    );

    fs::remove_dir_all(root.join("dir2")).unwrap();
    expect(watcher, &[removed("dir2", Dir)]);

    assert_eq!(watcher.snapshot(), &Snapshot::scan(root).unwrap());
}

#[test]
fn native_watcher_reports_live_changes_to_files_and_folders() {
    let dir = tempfile::tempdir().unwrap();
    let mut watcher = NativeWatcher::start(dir.path(), Snapshot::default()).unwrap();
    live_changes(&mut watcher, dir.path());
}

#[test]
fn rescan_watcher_reports_live_changes_to_files_and_folders() {
    let dir = tempfile::tempdir().unwrap();
    let mut watcher =
        RescanWatcher::new(dir.path(), Snapshot::default(), Duration::from_millis(20)).unwrap();
    live_changes(&mut watcher, dir.path());
}

#[test]
fn replacing_a_file_reports_a_modification() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("doc.txt"), "draft").unwrap();
    fs::write(root.join("thing"), "file").unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();

    fs::write(root.join(".doc.txt.tmp"), "final").unwrap();
    fs::rename(root.join(".doc.txt.tmp"), root.join("doc.txt")).unwrap();
    fs::remove_file(root.join("thing")).unwrap();
    fs::create_dir(root.join("thing")).unwrap();

    let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
    assert_eq!(
        changes,
        [
            removed("thing", File),
            created("thing", Dir),
            modified("doc.txt"),
        ]
    );
}

#[test]
fn an_entry_made_on_an_inode_another_entry_just_freed_is_not_its_move() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("a")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    for round in 0..200 {
        fs::write(root.join("a/old.txt"), "old").unwrap();
        fs::create_dir(root.join("a/old")).unwrap();
        let mut snapshot = Snapshot::scan(root).unwrap();
        fs::remove_file(root.join("a/old.txt")).unwrap();
        fs::write(root.join("b/new.txt"), "new").unwrap();
        fs::remove_dir(root.join("a/old")).unwrap();
        fs::create_dir(root.join("b/new")).unwrap();
        let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
        assert_eq!(
            changes,
            [
                removed("a/old", Dir),
                removed("a/old.txt", File),
                created("b/new", Dir),
                created("b/new.txt", File),
            ],
            "round {round}"
        );
        fs::remove_file(root.join("b/new.txt")).unwrap();
        fs::remove_dir(root.join("b/new")).unwrap();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_file_made_on_a_freed_inode_gets_another_handle() {
    use std::os::fd::AsFd;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    let id = || koidrive_fs::id_of(fs::File::open(&path).unwrap().as_fd()).unwrap();
    let mut reused = 0;
    for _ in 0..50 {
        fs::write(&path, "old").unwrap();
        let old = id();
        fs::remove_file(&path).unwrap();
        fs::write(&path, "new").unwrap();
        let new = id();
        fs::remove_file(&path).unwrap();
        if new.ino == old.ino
            && let Some(old_handle) = old.handle
        {
            reused += 1;
            assert_ne!(new.handle.unwrap(), old_handle);
        }
    }
    let ext4 = rustix::fs::statfs(dir.path()).unwrap().f_type == libc::EXT4_SUPER_MAGIC;
    assert!(reused > 0 || !ext4, "ext4 never reused an inode");
}

#[cfg(target_os = "linux")]
#[test]
fn an_identity_holding_another_entrys_handle_makes_its_move_a_removal_and_a_creation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.txt"), "a").unwrap();
    fs::write(root.join("b.txt"), "b").unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();
    let Some(other) = snapshot.entries[Path::new("b.txt")].id.handle else {
        return;
    };
    (snapshot.entries.get_mut(Path::new("a.txt")).unwrap().id).handle = Some(other);
    fs::rename(root.join("a.txt"), root.join("c.txt")).unwrap();
    let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
    assert_eq!(changes, [removed("a.txt", File), created("c.txt", File)]);
}

#[test]
fn rescan_refuses_a_missing_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "a").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let before = snapshot.clone();
    for scope in ["", "a.txt"] {
        assert!(matches!(
            snapshot.rescan(&root, [PathBuf::from(scope)]),
            Err(Error::Io(_))
        ));
        assert_eq!(snapshot, before);
    }
}

#[test]
fn case_only_rename_is_a_rename() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("letter.txt"), "l").unwrap();
    fs::create_dir(root.join("docs")).unwrap();
    fs::write(root.join("docs/a.txt"), "a").unwrap();
    let previous = Snapshot::scan(root).unwrap();

    for alias in ["LETTER.txt", "DOCS/a.txt"] {
        let mut snapshot = previous.clone();
        assert_eq!(
            snapshot.rescan(root, [PathBuf::from(alias)]).unwrap(),
            nothing(root)
        );
        assert_eq!(snapshot, previous, "{alias} is not a name on disk");
    }

    fs::rename(root.join("letter.txt"), root.join("LETTER.txt")).unwrap();
    fs::rename(root.join("docs"), root.join("DOCS")).unwrap();
    let expected = [
        renamed("docs", "DOCS", Dir),
        renamed("letter.txt", "LETTER.txt", File),
    ];
    for paths in [
        vec!["LETTER.txt", "DOCS/a.txt"],
        vec!["letter.txt", "docs/a.txt"],
        vec!["letter.txt", "LETTER.txt", "docs", "DOCS"],
    ] {
        let mut snapshot = previous.clone();
        let changes = snapshot
            .rescan(root, paths.iter().map(PathBuf::from))
            .unwrap()
            .changes;
        assert_eq!(changes, expected, "{paths:?}");
        assert_eq!(snapshot, Snapshot::scan(root).unwrap(), "{paths:?}");
    }
}

#[test]
fn normalization_alias_is_not_an_entry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("caf\u{e9}.txt"), "c").unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();
    let previous = snapshot.clone();
    assert_eq!(
        snapshot
            .rescan(root, [PathBuf::from("cafe\u{301}.txt")])
            .unwrap(),
        nothing(root)
    );
    assert_eq!(snapshot, previous);
}

#[test]
fn native_watcher_reports_a_case_only_rename() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut watcher = NativeWatcher::start(root, Snapshot::default()).unwrap();
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        quiet(root)
    );
    fs::write(root.join("letter.txt"), "l").unwrap();
    expect(&mut watcher, &[created("letter.txt", File)]);
    fs::rename(root.join("letter.txt"), root.join("LETTER.txt")).unwrap();
    expect(&mut watcher, &[renamed("letter.txt", "LETTER.txt", File)]);
    assert_eq!(watcher.snapshot(), &Snapshot::scan(root).unwrap());
}

#[test]
fn folder_replaced_by_a_symlink_is_removed_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let outside = dir.path().join("outside");
    fs::create_dir_all(root.join("d")).unwrap();
    fs::write(root.join("d/x"), "inside").unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("x"), "outside, longer").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();

    fs::remove_dir_all(root.join("d")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("d")).unwrap();

    assert_eq!(
        snapshot.rescan(&root, [PathBuf::from("d/x")]).unwrap(),
        Rescan {
            changes: vec![removed("d", Dir)],
            unsettled: vec![],
            unreadable: vec![],
            root: root_id(&root),
            rules: vec![],
            read: vec![],
            tracked: vec![],
            settled: vec![],
        }
    );
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap());
    assert!(snapshot.entries.is_empty());
}

#[test]
fn rescan_rejects_paths_outside_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let mut snapshot = Snapshot::scan(dir.path()).unwrap();
    for path in ["../x", "/etc", "a/../b"] {
        assert!(
            matches!(
                snapshot.rescan(dir.path(), [PathBuf::from(path)]),
                Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::InvalidInput
            ),
            "{path}"
        );
    }
}

#[test]
fn unreadable_folder_is_kept_and_reported() {
    for mode in [0o000, 0o600] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let locked = root.join("d/locked");
        fs::create_dir_all(locked.join("inner")).unwrap();
        fs::write(locked.join("x"), "x").unwrap();
        let mut snapshot = Snapshot::scan(root).unwrap();
        let known = snapshot.clone();
        fs::set_permissions(&locked, fs::Permissions::from_mode(mode)).unwrap();
        let probe = fs::symlink_metadata(locked.join("x"));
        if !blocks_reads("unreadable_folder_is_kept_and_reported", probe) {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        fs::write(root.join("new.txt"), "new").unwrap();
        let blocked = vec![PathBuf::from("d/locked")];

        assert_eq!(
            snapshot.rescan(root, [PathBuf::new()]).unwrap(),
            Rescan {
                changes: vec![created("new.txt", File)],
                unsettled: vec![],
                unreadable: blocked.clone(),
                root: root_id(root),
                rules: vec![],
                read: vec![],
                tracked: vec![],
                settled: vec![],
            },
            "{mode:o}"
        );
        for (path, entry) in &known.entries {
            assert_eq!(snapshot.entries.get(path), Some(entry), "{mode:o} {path:?}");
        }
        assert_eq!(
            snapshot
                .rescan(root, [PathBuf::from("d/locked/x")])
                .unwrap(),
            Rescan {
                unreadable: blocked.clone(),
                ..nothing(root)
            },
            "{mode:o}"
        );
        let first = Snapshot::scan(root).unwrap();
        assert!(
            first.entries.contains_key(Path::new("d/locked")),
            "{mode:o}"
        );
        assert!(
            !first.entries.contains_key(Path::new("d/locked/x")),
            "{mode:o}"
        );
        let mut watcher =
            RescanWatcher::new(root, snapshot.clone(), Duration::from_millis(50)).unwrap();
        assert_eq!(
            watcher.poll(Duration::from_millis(100)).unwrap(),
            Poll {
                unreadable: blocked.clone(),
                ..quiet(root)
            },
            "{mode:o}"
        );

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            snapshot.rescan(root, [PathBuf::new()]).unwrap(),
            nothing(root),
            "{mode:o}"
        );
        assert_eq!(snapshot, Snapshot::scan(root).unwrap(), "{mode:o}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_watcher_starts_beside_an_unreadable_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("d/locked/inner")).unwrap();
    fs::create_dir(root.join("e")).unwrap();
    let snapshot = Snapshot::scan(root).unwrap();
    fs::set_permissions(root.join("d/locked"), fs::Permissions::from_mode(0o000)).unwrap();
    let probe = fs::read_dir(root.join("d/locked"));
    if !blocks_reads("native_watcher_starts_beside_an_unreadable_folder", probe) {
        fs::set_permissions(root.join("d/locked"), fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let watcher = NativeWatcher::start(root, snapshot);
    let result = watcher.map(|mut watcher| {
        let first = watcher.poll(Duration::from_millis(300)).unwrap();
        fs::write(root.join("e/new.txt"), "new").unwrap();
        expect(&mut watcher, &[created("e/new.txt", File)]);
        first
    });
    fs::set_permissions(root.join("d/locked"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        result.unwrap(),
        Poll {
            unreadable: vec![PathBuf::from("d/locked")],
            ..quiet(root)
        }
    );
}

#[cfg(target_os = "linux")]
#[test]
fn folder_readable_again_is_watched_again() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let locked = root.join("d/locked");
    fs::create_dir_all(locked.join("inner")).unwrap();
    let snapshot = Snapshot::scan(root).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();
    let probe = fs::symlink_metadata(locked.join("inner"));
    if !blocks_reads("folder_readable_again_is_watched_again", probe) {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let mut watcher = NativeWatcher::start(root, snapshot).unwrap();
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        Poll {
            unreadable: vec![PathBuf::from("d/locked")],
            ..quiet(root)
        }
    );
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let quiet = Instant::now() + Duration::from_secs(2);
    while Instant::now() < quiet {
        watcher.poll(Duration::from_millis(300)).unwrap();
    }
    fs::write(locked.join("inner/new.txt"), "new").unwrap();
    expect(&mut watcher, &[created("d/locked/inner/new.txt", File)]);
}

#[test]
fn private_folder_is_never_reported() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let private = root.join(koidrive_fs::PRIVATE_DIR);
    fs::create_dir_all(private.join("staging")).unwrap();
    fs::write(private.join("space.json"), "{}").unwrap();
    fs::write(root.join("a.txt"), "a").unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();
    assert_eq!(
        snapshot.entries.keys().collect::<Vec<_>>(),
        [Path::new("a.txt")]
    );

    fs::write(private.join("staging/x"), "x").unwrap();
    fs::remove_file(private.join("space.json")).unwrap();
    for scope in [
        "",
        ".koidrive",
        ".koidrive/staging/x",
        ".koidrive/space.json",
    ] {
        assert_eq!(
            snapshot.rescan(root, [PathBuf::from(scope)]).unwrap(),
            nothing(root),
            "{scope:?}"
        );
    }

    let mut watcher = NativeWatcher::start(root, snapshot.clone()).unwrap();
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        quiet(root)
    );
    fs::write(private.join("staging/y"), "y").unwrap();
    fs::write(root.join("b.txt"), "b").unwrap();
    expect(&mut watcher, &[created("b.txt", File)]);
    assert_eq!(watcher.snapshot(), &Snapshot::scan(root).unwrap());
}
#[test]
fn forgotten_scope_is_reported_again_by_the_next_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("d/sub")).unwrap();
    fs::write(root.join("d/x"), "x").unwrap();
    fs::write(root.join("d/sub/y"), "y").unwrap();
    fs::create_dir(root.join("e")).unwrap();
    fs::write(root.join("e/z"), "z").unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();
    let full = snapshot.clone();

    assert_eq!(
        snapshot.rescan(root, [PathBuf::from("d")]).unwrap(),
        nothing(root)
    );

    snapshot.reset(Path::new("d/sub"), BTreeMap::new());
    let known: Vec<&Path> = snapshot.entries.keys().map(PathBuf::as_path).collect();
    assert_eq!(known, ["d", "d/x", "e", "e/z"].map(Path::new));
    assert_eq!(
        snapshot.rescan(root, [PathBuf::from("d/sub")]).unwrap(),
        Rescan {
            changes: vec![created("d/sub", Dir), created("d/sub/y", File)],
            ..nothing(root)
        }
    );
    assert_eq!(snapshot, full);

    snapshot.reset(Path::new("d"), BTreeMap::new());
    assert_eq!(
        snapshot.rescan(root, [PathBuf::from("d")]).unwrap(),
        Rescan {
            changes: vec![
                created("d", Dir),
                created("d/sub", Dir),
                created("d/sub/y", File),
                created("d/x", File),
            ],
            ..nothing(root)
        }
    );
    assert_eq!(snapshot, full);

    snapshot.reset(Path::new(""), BTreeMap::new());
    assert!(snapshot.entries.is_empty());
}

/// A tree with a folder `d` to reset and a folder `e` beside it.
fn two_folders() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("d/sub")).unwrap();
    fs::write(root.join("d/x"), "x").unwrap();
    fs::write(root.join("d/sub/y"), "y").unwrap();
    fs::create_dir(root.join("e")).unwrap();
    fs::write(root.join("e/z"), "z").unwrap();
    dir
}

/// Resets `d` to nothing in a watcher that has caught up with the tree, then
/// polls without waiting for events. Nothing on disk changed, so the poll can
/// only report `d` because the watcher rescans it on its own.
fn forgotten_scope_is_reported(watcher: &mut dyn Watcher, root: &Path) {
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        quiet(root)
    );

    watcher.reset(Path::new("d"), BTreeMap::new());
    assert_eq!(
        watcher.poll(Duration::ZERO).unwrap(),
        Poll {
            changes: vec![
                created("d", Dir),
                created("d/sub", Dir),
                created("d/sub/y", File),
                created("d/x", File),
            ],
            ..quiet(root)
        }
    );
    assert_eq!(watcher.snapshot(), &Snapshot::scan(root).unwrap());
}

#[test]
fn native_watcher_rescans_a_forgotten_scope_without_an_event() {
    let dir = two_folders();
    let root = fs::canonicalize(dir.path()).unwrap();
    let mut watcher = NativeWatcher::start(&root, Snapshot::scan(&root).unwrap()).unwrap();
    forgotten_scope_is_reported(&mut watcher, &root);
}

#[test]
fn rescan_watcher_reports_only_a_forgotten_scope() {
    let dir = two_folders();
    let root = fs::canonicalize(dir.path()).unwrap();
    let mut watcher = RescanWatcher::new(
        &root,
        Snapshot::scan(&root).unwrap(),
        Duration::from_millis(20),
    )
    .unwrap();
    forgotten_scope_is_reported(&mut watcher, &root);
}

/// A tree as `two_folders` makes it, and what was under `d` before
/// `d/gone` was deleted from it.
fn tree_that_lost_a_file() -> (tempfile::TempDir, PathBuf, BTreeMap<PathBuf, Known>) {
    let dir = two_folders();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::write(root.join("d/gone"), "gone").unwrap();
    let known = Snapshot::scan(&root)
        .unwrap()
        .entries
        .into_iter()
        .filter(|(path, _)| path.starts_with("d"))
        .map(|(path, Entry { kind, id, .. })| (path, Known { kind, id }))
        .collect();
    fs::remove_file(root.join("d/gone")).unwrap();
    (dir, root, known)
}

/// Resets `d` to entries that include `d/gone`, which the disk lacks, in a
/// watcher that has caught up with the tree. The next poll reports that
/// removal, and the two files `d` still holds as modified although nothing
/// changed them: the reset does not say what they hold.
fn reset_entry_the_disk_lacks_is_removed(
    watcher: &mut dyn Watcher,
    root: &Path,
    known: BTreeMap<PathBuf, Known>,
) {
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        quiet(root)
    );

    watcher.reset(Path::new("d"), known);
    assert_eq!(
        watcher.poll(Duration::ZERO).unwrap(),
        Poll {
            changes: vec![
                removed("d/gone", File),
                modified("d/sub/y"),
                modified("d/x")
            ],
            ..quiet(root)
        }
    );
    assert_eq!(watcher.snapshot(), &Snapshot::scan(root).unwrap());
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap().changes,
        [],
        "a file is reported once per reset"
    );
}

#[test]
fn native_watcher_reports_a_reset_entry_the_disk_lacks_as_removed() {
    let (_dir, root, known) = tree_that_lost_a_file();
    let mut watcher = NativeWatcher::start(&root, Snapshot::scan(&root).unwrap()).unwrap();
    reset_entry_the_disk_lacks_is_removed(&mut watcher, &root, known);
}

#[test]
fn rescan_watcher_reports_a_reset_entry_the_disk_lacks_as_removed() {
    let (_dir, root, known) = tree_that_lost_a_file();
    let mut watcher = RescanWatcher::new(
        &root,
        Snapshot::scan(&root).unwrap(),
        Duration::from_millis(20),
    )
    .unwrap();
    reset_entry_the_disk_lacks_is_removed(&mut watcher, &root, known);
}

#[test]
fn rescans_and_polls_report_the_root_they_scanned() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "a").unwrap();
    let first = root_id(&root);
    let mut snapshot = Snapshot::scan(&root).unwrap();
    assert_eq!(
        snapshot.rescan(&root, [PathBuf::new()]).unwrap().root,
        first
    );
    let mut native = NativeWatcher::start(&root, snapshot.clone()).unwrap();
    assert_eq!(
        native.poll(Duration::from_millis(300)).unwrap(),
        quiet(&root)
    );
    drop(native);
    let mut watcher =
        RescanWatcher::new(&root, snapshot.clone(), Duration::from_millis(20)).unwrap();
    assert_eq!(
        watcher.poll(Duration::from_millis(50)).unwrap().root,
        Some(first)
    );

    fs::rename(&root, dir.path().join("moved")).unwrap();
    fs::create_dir(&root).unwrap();
    let second = root_id(&root);
    assert_ne!(first, second);
    assert_eq!(
        snapshot.rescan(&root, [PathBuf::new()]).unwrap(),
        Rescan {
            changes: vec![removed("a.txt", File)],
            unsettled: vec![],
            unreadable: vec![],
            root: second,
            rules: vec![],
            read: vec![],
            tracked: vec![],
            settled: vec![],
        }
    );
    let poll = watcher.poll(Duration::from_millis(50)).unwrap();
    assert_eq!(poll.changes, [removed("a.txt", File)]);
    assert_eq!(poll.root, Some(second));
}

#[test]
fn a_folder_moved_over_an_empty_folder_is_renamed_onto_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("drafts/inner")).unwrap();
    fs::write(root.join("drafts/a.txt"), "a").unwrap();
    fs::write(root.join("drafts/inner/b.txt"), "b").unwrap();
    fs::create_dir(root.join("final")).unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();

    fs::rename(root.join("drafts"), root.join("final")).unwrap();
    let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
    assert_eq!(
        changes,
        [removed("final", Dir), renamed("drafts", "final", Dir)]
    );
    assert_eq!(snapshot, Snapshot::scan(root).unwrap());
}

/// A folder that held entries is never replaced by another one, so a file
/// that keeps its path while the folder around it changes stays where it is.
#[test]
fn a_folder_that_held_entries_is_not_replaced() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("space");
    fs::create_dir_all(root.join("d")).unwrap();
    fs::write(root.join("d/x"), "x").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(root.join("d/x"), root.join("x")).unwrap();
    fs::rename(root.join("d"), outer.path().join("away")).unwrap();
    fs::create_dir(root.join("d")).unwrap();
    fs::rename(root.join("x"), root.join("d/x")).unwrap();
    let changes = snapshot.rescan(&root, [PathBuf::new()]).unwrap().changes;
    assert_eq!(changes, [reidentified("d")]);
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap());

    let root = outer.path().join("moved");
    fs::create_dir_all(root.join("d")).unwrap();
    fs::write(root.join("d/x"), "x").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(root.join("d"), root.join("d.old")).unwrap();
    fs::create_dir(root.join("d")).unwrap();
    fs::rename(root.join("d.old/x"), root.join("d/x")).unwrap();
    let changes = snapshot.rescan(&root, [PathBuf::new()]).unwrap().changes;
    assert_eq!(changes, [created("d.old", Dir), reidentified("d")]);
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap());
}

/// A folder moved to another path and a new folder made at its old path are a
/// rename and a creation, whether the folder was empty or its entries moved
/// with it. Otherwise the new folder would take the moved folder's place and
/// the moved one would read as created with an identity the snapshot gave away.
#[test]
fn a_folder_moved_away_and_made_again_at_its_path_is_renamed_and_created() {
    for held in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("a")).unwrap();
        if held {
            fs::write(root.join("a/f"), "f").unwrap();
        }
        let mut snapshot = Snapshot::scan(root).unwrap();
        fs::rename(root.join("a"), root.join("b")).unwrap();
        fs::create_dir(root.join("a")).unwrap();
        let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
        assert_eq!(
            changes,
            [renamed("a", "b", Dir), created("a", Dir)],
            "held: {held}"
        );
        assert_eq!(snapshot, Snapshot::scan(root).unwrap(), "held: {held}");
    }
}

/// A folder moved away and made again at its path, with a new folder made at
/// the path of one it held, which is then removed from the moved folder: the
/// new folder below is a creation too, and the one it replaced is removed.
/// Otherwise the old folder's node would move along to a path the disk does
/// not hold, and the new one would never be reported.
#[test]
fn a_new_folder_below_a_folder_made_again_at_its_path_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("a/s")).unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();
    fs::rename(root.join("a"), root.join("b")).unwrap();
    fs::create_dir_all(root.join("a/s")).unwrap();
    fs::remove_dir(root.join("b/s")).unwrap();
    let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
    assert_eq!(
        changes,
        [
            removed("a/s", Dir),
            renamed("a", "b", Dir),
            created("a", Dir),
            created("a/s", Dir)
        ]
    );
    assert_eq!(snapshot, Snapshot::scan(root).unwrap());
}

#[test]
fn a_folder_on_another_device_number_is_not_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("kept")).unwrap();
    fs::write(root.join("kept/a.txt"), "a").unwrap();
    let scanned = Snapshot::scan(root).unwrap();

    for ino in [0, 1000] {
        let mut snapshot = scanned.clone();
        for entry in snapshot.entries.values_mut() {
            entry.id.ino += ino;
        }
        let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
        let expected = match ino {
            0 => vec![],
            _ => vec![modified("kept/a.txt"), reidentified("kept")],
        };
        assert_eq!(changes, expected, "inode moved by {ino}");
        assert_eq!(snapshot, scanned, "inode moved by {ino}");
    }
}

/// A folder moved onto the path of a folder that moved elsewhere in the same
/// rescan, as in a rotation through a third name, is a rename, although the
/// folder that held the path held entries.
#[test]
fn a_folder_moved_onto_the_path_of_a_folder_that_moved_away_is_renamed_onto_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a"), "a").unwrap();
    fs::create_dir(root.join("b")).unwrap();
    fs::write(root.join("b/one"), "one").unwrap();
    fs::create_dir(root.join("c")).unwrap();
    fs::write(root.join("c/two"), "two").unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();

    for (from, to) in [("a", "tmp"), ("c", "a"), ("b", "c"), ("tmp", "b")] {
        fs::rename(root.join(from), root.join(to)).unwrap();
    }
    let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
    assert_eq!(
        changes,
        [
            renamed("a", "b", File),
            renamed("b", "c", Dir),
            renamed("c", "a", Dir)
        ]
    );
    assert_eq!(snapshot, Snapshot::scan(root).unwrap());
}

/// Files and folders that swap paths through a third name are renames, so the
/// engine keeps each entry's node. A folder holds an entry of the same name as
/// the other one, as a swap of two project folders does.
#[test]
fn entries_swapped_through_a_third_name_are_renamed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("one"), "one").unwrap();
    fs::write(root.join("two"), "two").unwrap();
    for folder in ["x", "y"] {
        fs::create_dir(root.join(folder)).unwrap();
        fs::write(root.join(folder).join("note"), folder).unwrap();
    }
    let mut snapshot = Snapshot::scan(root).unwrap();

    for (first, second) in [("one", "two"), ("x", "y")] {
        for (from, to) in [(first, "tmp"), (second, first), ("tmp", second)] {
            fs::rename(root.join(from), root.join(to)).unwrap();
        }
    }
    let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
    assert_eq!(
        changes,
        [
            renamed("one", "two", File),
            renamed("two", "one", File),
            renamed("x", "y", Dir),
            renamed("y", "x", Dir)
        ]
    );
    assert_eq!(snapshot, Snapshot::scan(root).unwrap());
}

fn rescanned_in_two_batches(renames: &[(&str, &str)], first: &[&str], later: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for file in ["one", "two", "a", "b", "c"] {
        fs::write(root.join(file), file).unwrap();
    }
    for folder in ["x", "y"] {
        fs::create_dir(root.join(folder)).unwrap();
        fs::write(root.join(folder).join("note"), folder).unwrap();
    }
    let mut snapshot = Snapshot::scan(root).unwrap();
    let mut whole = snapshot.clone();
    for (from, to) in renames {
        fs::rename(root.join(from), root.join(to)).unwrap();
    }
    let expected = whole.rescan(root, [PathBuf::new()]).unwrap().changes;
    let scopes = first.iter().map(PathBuf::from);
    assert_eq!(snapshot.rescan(root, scopes).unwrap().changes, expected);
    assert_eq!(snapshot, whole);
    let scopes = later.iter().map(PathBuf::from);
    assert_eq!(snapshot.rescan(root, scopes).unwrap().changes, []);
}

#[test]
fn two_files_swapped_in_two_event_batches_are_renamed() {
    rescanned_in_two_batches(
        &[("one", "tmp"), ("two", "one"), ("tmp", "two")],
        &["one", "tmp"],
        &["two"],
    );
}

#[test]
fn two_folders_swapped_in_two_event_batches_are_renamed() {
    rescanned_in_two_batches(
        &[("x", "tmp"), ("y", "x"), ("tmp", "y")],
        &["x", "tmp"],
        &["y"],
    );
}

#[test]
fn three_files_rotated_in_two_event_batches_are_renamed() {
    rescanned_in_two_batches(
        &[("a", "tmp"), ("c", "a"), ("b", "c"), ("tmp", "b")],
        &["a", "tmp"],
        &["b", "c"],
    );
}

#[test]
fn a_file_moved_over_another_in_two_event_batches_reads_as_in_one_batch() {
    rescanned_in_two_batches(&[("x/note", "one")], &["one"], &["x/note"]);
}

#[test]
fn a_file_moved_into_another_folder_in_two_event_batches_is_renamed() {
    rescanned_in_two_batches(&[("x/note", "y/moved")], &["y/moved"], &["x/note"]);
}

#[test]
fn files_moved_back_to_their_paths_after_their_folders_swap_are_renamed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (folder, file) in [("a", "1"), ("b", "2")] {
        fs::create_dir_all(root.join(folder).join("s")).unwrap();
        fs::write(root.join(folder).join("s").join(file), file).unwrap();
    }
    let mut snapshot = Snapshot::scan(root).unwrap();

    for (from, to) in [
        ("a", "t"),
        ("b", "a"),
        ("t", "b"),
        ("a/s/2", "b/s/2"),
        ("b/s/1", "a/s/1"),
    ] {
        fs::rename(root.join(from), root.join(to)).unwrap();
    }
    let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
    assert_eq!(
        changes,
        [
            renamed("a", "b", Dir),
            renamed("a/s/1", "a/s/1", File),
            renamed("b", "a", Dir),
            renamed("b/s/2", "b/s/2", File)
        ]
    );
    assert_eq!(snapshot, Snapshot::scan(root).unwrap());
}

/// A file written over by renaming a new file onto it keeps its path: the
/// old file is gone, so nothing was swapped.
#[test]
fn a_file_replaced_by_a_rename_is_modified_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("doc"), "first").unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();

    fs::write(root.join(".doc.tmp"), "second").unwrap();
    fs::rename(root.join(".doc.tmp"), root.join("doc")).unwrap();
    let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
    assert_eq!(changes, [Change::Modified { path: "doc".into() }]);
}

#[test]
fn an_in_place_edit_that_keeps_the_size_and_restores_the_mtime_is_modified() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("doc"), "first").unwrap();
    let written = fs::metadata(root.join("doc")).unwrap().modified().unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();
    while matches!(snapshot.entries[Path::new("doc")].ctime, Ctime::Racy { .. }) {
        assert!(
            snapshot
                .rescan(root, [PathBuf::new()])
                .unwrap()
                .changes
                .is_empty()
        );
    }

    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(root.join("doc"))
        .unwrap();
    file.write_all(b"other").unwrap();
    file.set_modified(written).unwrap();
    let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap().changes;
    assert_eq!(changes, [modified("doc")]);
}

#[test]
fn a_file_written_on_fat_right_before_a_scan_is_racy_until_its_tick_passes() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let stick = disks.base().join("Stick");
    fs::create_dir(&stick).unwrap();
    let image = disks.fat("Stick");
    disks.attach(&image, &stick);
    let into_tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        % 2000;
    std::thread::sleep(Duration::from_millis(((2050 - into_tick) % 2000) as u64));
    fs::write(stick.join("doc"), "first").unwrap();
    let started = Instant::now();
    let mut snapshot = Snapshot::scan(&stick).unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    let racy = |snapshot: &Snapshot| {
        matches!(snapshot.entries[Path::new("doc")].ctime, Ctime::Racy { .. })
    };
    assert!(racy(&snapshot));
    std::thread::sleep(Duration::from_millis(2100));
    let changes = snapshot.rescan(&stick, [PathBuf::new()]).unwrap().changes;
    assert!(!racy(&snapshot));
    assert!(!changes.contains(&modified("doc")), "{changes:?}");
}

#[test]
fn a_mark_on_a_folder_rescans_nothing_below_it() {
    #[cfg(target_vendor = "apple")]
    const MARK: &str = "com.koidrive.node";
    #[cfg(not(target_vendor = "apple"))]
    const MARK: &str = "user.koidrive.node";
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    for folder in 0..10 {
        let folder = root.join(format!("deps/p{folder}"));
        fs::create_dir_all(&folder).unwrap();
        for file in 0..10 {
            fs::write(folder.join(format!("f{file}")), "x").unwrap();
        }
    }
    let found = Arc::new(AtomicU64::new(0));
    let counted = found.clone();
    let mut watcher = NativeWatcher::start(&root, Snapshot::scan(&root).unwrap())
        .unwrap()
        .counting(move |files| {
            counted.fetch_max(files, Ordering::Relaxed);
        });
    let deadline = Instant::now() + WAIT;
    loop {
        found.store(0, Ordering::Relaxed);
        watcher.poll(Duration::from_millis(500)).unwrap();
        if found.load(Ordering::Relaxed) == 0 || Instant::now() > deadline {
            break;
        }
    }

    rustix::fs::setxattr(
        root.join("deps"),
        MARK,
        b"n1",
        rustix::fs::XattrFlags::empty(),
    )
    .unwrap();
    for _ in 0..4 {
        assert_eq!(
            watcher.poll(Duration::from_millis(500)).unwrap().changes,
            []
        );
    }
    assert_eq!(found.load(Ordering::Relaxed), 0);

    fs::write(root.join("deps/p3/f2"), "changed").unwrap();
    expect(&mut watcher, &[modified("deps/p3/f2")]);
}

#[cfg(target_os = "linux")]
#[test]
fn folder_made_unreadable_while_watched_is_reported() {
    for mode in [0o000, 0o444] {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let locked = root.join("d/locked");
        fs::create_dir_all(locked.join("inner")).unwrap();
        fs::write(locked.join("x"), "x").unwrap();
        let mut watcher = NativeWatcher::start(&root, Snapshot::scan(&root).unwrap()).unwrap();
        watcher.poll(Duration::from_millis(300)).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(mode)).unwrap();
        let probe = fs::symlink_metadata(locked.join("x"));
        if !blocks_reads("folder_made_unreadable_while_watched_is_reported", probe) {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let mut unreadable = vec![];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && unreadable.is_empty() {
            unreadable = watcher.poll(Duration::from_millis(300)).unwrap().unreadable;
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(unreadable, vec![PathBuf::from("d/locked")], "mode {mode:o}");
    }
}

#[test]
fn a_folder_whose_mode_changes_is_scanned_once_and_then_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("d/inner")).unwrap();
    fs::write(root.join("d/x"), "x").unwrap();
    let mut watcher = NativeWatcher::start(&root, Snapshot::scan(&root).unwrap()).unwrap();
    let deadline = Instant::now() + WAIT;
    while watcher
        .poll(Duration::from_millis(500))
        .unwrap()
        .root
        .is_some()
    {
        assert!(Instant::now() < deadline, "the first scan never settled");
    }

    fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o700)).unwrap();
    let deadline = Instant::now() + WAIT;
    while watcher
        .poll(Duration::from_millis(300))
        .unwrap()
        .root
        .is_none()
    {
        assert!(Instant::now() < deadline, "the mode change woke no scan");
    }
    let deadline = Instant::now() + WAIT;
    while watcher
        .poll(Duration::from_millis(500))
        .unwrap()
        .root
        .is_some()
    {
        assert!(
            Instant::now() < deadline,
            "every poll still scans after the mode change"
        );
    }
}

fn moved_out_of_a_removed_folder() -> (tempfile::TempDir, PathBuf, Snapshot) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("x/d/sub")).unwrap();
    fs::create_dir(root.join("e")).unwrap();
    fs::write(root.join("x/d/g.txt"), "g").unwrap();
    fs::write(root.join("x/d/sub/s.txt"), "s").unwrap();
    fs::write(root.join("x/d/other.txt"), "o").unwrap();
    let snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(root.join("x/d/g.txt"), root.join("e/moved.txt")).unwrap();
    fs::rename(root.join("x/d/sub"), root.join("e/sub")).unwrap();
    fs::remove_dir_all(root.join("x/d")).unwrap();
    (dir, root, snapshot)
}

fn moves_out_of_the_removed_folder() -> Vec<Change> {
    vec![
        removed("x/d", Dir),
        renamed("x/d/g.txt", "e/moved.txt", File),
        renamed("x/d/sub", "e/sub", Dir),
    ]
}

#[test]
fn a_scan_of_the_place_a_file_moved_to_reports_the_move_from_a_place_it_did_not_scan() {
    let (_dir, root, mut snapshot) = moved_out_of_a_removed_folder();
    let rescan = snapshot.rescan(&root, [PathBuf::from("e")]).unwrap();
    assert_eq!(rescan.changes, moves_out_of_the_removed_folder());
    assert!(rescan.unsettled.is_empty());
    assert_eq!(
        snapshot.rescan(&root, [PathBuf::from("x")]).unwrap(),
        nothing(&root)
    );
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn a_scan_of_the_place_a_file_moved_from_reports_the_move_to_a_place_it_did_not_scan() {
    let (_dir, root, mut snapshot) = moved_out_of_a_removed_folder();
    let rescan = snapshot.rescan(&root, [PathBuf::from("x")]).unwrap();
    assert_eq!(rescan.changes, moves_out_of_the_removed_folder());
    assert!(rescan.unsettled.is_empty());
    assert_eq!(
        snapshot.rescan(&root, [PathBuf::from("e")]).unwrap(),
        nothing(&root)
    );
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap());
}

#[test]
fn a_new_link_a_scan_finds_beside_the_file_it_links_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("x")).unwrap();
    fs::create_dir(root.join("e")).unwrap();
    fs::write(root.join("x/g.txt"), "g").unwrap();
    fs::set_permissions(root.join("x/g.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::hard_link(root.join("x/g.txt"), root.join("e/link.txt")).unwrap();
    let rescan = snapshot.rescan(&root, [PathBuf::from("e")]).unwrap();
    assert_eq!(rescan.changes, vec![created("e/link.txt", File)]);
    assert!(snapshot.entries.contains_key(Path::new("x/g.txt")));
    fs::remove_file(root.join("e/link.txt")).unwrap();
    let rescan = snapshot.rescan(&root, [PathBuf::new()]).unwrap();
    assert_eq!(
        rescan.changes,
        vec![removed("e/link.txt", File), modified("x/g.txt")]
    );
    fs::set_permissions(root.join("x/g.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        snapshot.rescan(&root, [PathBuf::new()]).unwrap().changes,
        vec![]
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_file_moved_into_a_folder_the_scan_may_not_read_keeps_its_old_place_until_the_folder_is_read() {
    let (_dir, root, mut snapshot) = moved_out_of_a_removed_folder();
    fs::set_permissions(root.join("e"), fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = blocks_reads(
        "a_file_moved_into_a_folder_the_scan_may_not_read_keeps_its_old_place_until_the_folder_is_read",
        fs::read_dir(root.join("e")),
    );
    let while_blocked =
        blocked.then(|| [0, 1].map(|_| snapshot.rescan(&root, [PathBuf::from("x")])));
    fs::set_permissions(root.join("e"), fs::Permissions::from_mode(0o755)).unwrap();
    let Some(while_blocked) = while_blocked else {
        return;
    };
    for rescan in while_blocked {
        let rescan = rescan.unwrap();
        assert_eq!(rescan.changes, vec![]);
        assert_eq!(rescan.unsettled, vec![PathBuf::from("x/d")]);
    }
    assert!(snapshot.entries.contains_key(Path::new("x/d/g.txt")));

    let rescan = snapshot.rescan(&root, [PathBuf::from("e")]).unwrap();
    assert_eq!(rescan.changes, moves_out_of_the_removed_folder());
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap());
}

fn moved_into_a_folder_renamed_after() -> (tempfile::TempDir, PathBuf, Snapshot) {
    let (dir, root, snapshot) = moved_out_of_a_removed_folder();
    fs::rename(root.join("e"), root.join("f")).unwrap();
    (dir, root, snapshot)
}

fn moves_into_the_renamed_folder() -> Vec<Change> {
    vec![
        removed("x/d", Dir),
        renamed("e", "f", Dir),
        renamed("x/d/g.txt", "f/moved.txt", File),
        renamed("x/d/sub", "f/sub", Dir),
    ]
}

#[test]
fn a_scan_of_a_renamed_folder_that_files_moved_into_reports_every_move() {
    let (_dir, root, mut snapshot) = moved_into_a_folder_renamed_after();
    let rescan = snapshot.rescan(&root, [PathBuf::from("f")]).unwrap();
    assert_eq!(rescan.changes, moves_into_the_renamed_folder());
    assert_eq!(
        snapshot.rescan(&root, [PathBuf::from("x")]).unwrap(),
        nothing(&root)
    );
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn a_scan_of_the_place_files_moved_from_into_a_folder_renamed_after_reports_every_move() {
    let (_dir, root, mut snapshot) = moved_into_a_folder_renamed_after();
    let rescan = snapshot.rescan(&root, [PathBuf::from("x")]).unwrap();
    assert_eq!(rescan.changes, moves_into_the_renamed_folder());
    assert_eq!(
        snapshot.rescan(&root, [PathBuf::from("f")]).unwrap(),
        nothing(&root)
    );
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap());
}

fn moved_and_removed() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("x/d")).unwrap();
    fs::create_dir(root.join("e")).unwrap();
    fs::write(root.join("x/d/g.txt"), "g").unwrap();
    (dir, root)
}

#[test]
fn a_file_removed_beside_a_link_is_removed_by_the_first_scan() {
    let (_dir, root) = moved_and_removed();
    fs::hard_link(root.join("x/d/g.txt"), root.join("x/alias.txt")).unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::remove_dir_all(root.join("x/d")).unwrap();
    let rescan = snapshot.rescan(&root, [PathBuf::from("x")]).unwrap();
    assert_eq!(
        rescan.changes,
        vec![removed("x/d", Dir), modified("x/alias.txt")]
    );
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn a_file_moved_into_a_folder_the_scan_may_only_search_is_a_rename_once_the_folder_is_read() {
    let (_dir, root) = moved_and_removed();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(root.join("x/d/g.txt"), root.join("e/moved.txt")).unwrap();
    fs::remove_dir(root.join("x/d")).unwrap();
    fs::set_permissions(root.join("e"), fs::Permissions::from_mode(0o111)).unwrap();
    let blocked = blocks_reads(
        "a_file_moved_into_a_folder_the_scan_may_only_search_is_a_rename_once_the_folder_is_read",
        fs::read_dir(root.join("e")),
    );
    let while_blocked = blocked.then(|| snapshot.rescan(&root, [PathBuf::from("x")]));
    fs::set_permissions(root.join("e"), fs::Permissions::from_mode(0o755)).unwrap();
    let Some(while_blocked) = while_blocked else {
        return;
    };
    let while_blocked = while_blocked.unwrap();
    let rescan = snapshot.rescan(&root, [PathBuf::from("e")]).unwrap();
    let changes: Vec<Change> = (while_blocked.changes.into_iter())
        .chain(rescan.changes)
        .collect();
    assert!(
        changes.contains(&renamed("x/d/g.txt", "e/moved.txt", File)),
        "{changes:?}"
    );
}

#[test]
fn a_folder_moved_over_an_empty_folder_through_the_native_watcher_keeps_its_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("drafts/inner")).unwrap();
    fs::write(root.join("drafts/inner/two.txt"), "two").unwrap();
    fs::create_dir(root.join("final")).unwrap();
    let snapshot = Snapshot::scan(&root).unwrap();
    let drafts = snapshot.entries[Path::new("drafts")].id;
    let mut watcher = NativeWatcher::start(&root, snapshot).unwrap();
    watcher.poll(Duration::from_millis(200)).unwrap();

    fs::rename(root.join("drafts"), root.join("final")).unwrap();
    expect(
        &mut watcher,
        &[removed("final", Dir), renamed("drafts", "final", Dir)],
    );
    assert_eq!(watcher.snapshot().entries[Path::new("final")].id, drafts);
}

#[test]
fn an_entry_moved_out_of_a_folder_that_then_becomes_unreadable_is_a_rename() {
    for (to, inside) in [
        ("e", "sub/inner"),
        ("y", "sub/inner"),
        ("e", "sub"),
        ("y", "sub"),
    ] {
        for (locked, scope) in [("x/d", ""), ("x/d", to), ("x", ""), ("x", to)] {
            moved_out_of_a_folder_that_then_becomes_unreadable(to, inside, locked, scope);
        }
    }
}

fn moved_out_of_a_folder_that_then_becomes_unreadable(
    to: &str,
    inside: &str,
    locked: &str,
    scope: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("x/d/sub/inner")).unwrap();
    fs::create_dir(root.join(to)).unwrap();
    fs::write(root.join("x/d/f.txt"), "f").unwrap();
    fs::write(root.join("x/d/other.txt"), "o").unwrap();
    fs::write(root.join("x/d/sub/inner/s.txt"), "s").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    let (doc, sub) = (format!("{to}/doc.txt"), format!("{to}/sub"));
    fs::rename(root.join("x/d/f.txt"), root.join(&doc)).unwrap();
    fs::rename(root.join("x/d/sub"), root.join(&sub)).unwrap();
    let locked = [root.join(locked), root.join(to).join(inside)];
    for path in &locked {
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    }
    let blocked = blocks_reads(
        "an_entry_moved_out_of_a_folder_that_then_becomes_unreadable_is_a_rename",
        fs::read_dir(&locked[0]),
    );
    let while_blocked = blocked.then(|| {
        let moved = snapshot.rescan(&root, [PathBuf::from(scope)]);
        fs::write(root.join(&doc), "edited").unwrap();
        (moved, snapshot.rescan(&root, [PathBuf::from(scope)]))
    });
    for path in &locked {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let Some((moved, edited)) = while_blocked else {
        return;
    };
    let case = format!("{locked:?} locked, scope {scope:?}");
    assert_eq!(
        moved.unwrap().changes,
        vec![
            renamed("x/d/f.txt", &doc, File),
            renamed("x/d/sub", &sub, Dir)
        ],
        "{case}"
    );
    assert_eq!(edited.unwrap().changes, vec![modified(&doc)], "{case}");
    assert!(snapshot.entries.contains_key(Path::new("x/d/other.txt")));
    assert_eq!(
        snapshot.rescan(&root, [PathBuf::new()]).unwrap(),
        nothing(&root),
        "{case}"
    );
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap(), "{case}");
}

#[test]
fn a_link_made_to_a_file_in_a_folder_that_then_becomes_unreadable_is_created_beside_it() {
    let (_dir, root) = moved_and_removed();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::hard_link(root.join("x/d/g.txt"), root.join("e/link.txt")).unwrap();
    fs::set_permissions(root.join("x/d"), fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = blocks_reads(
        "a_link_made_to_a_file_in_a_folder_that_then_becomes_unreadable_is_created_beside_it",
        fs::read_dir(root.join("x/d")),
    );
    let while_blocked = blocked.then(|| snapshot.rescan(&root, [PathBuf::new()]));
    fs::set_permissions(root.join("x/d"), fs::Permissions::from_mode(0o755)).unwrap();
    let Some(while_blocked) = while_blocked else {
        return;
    };
    assert_eq!(
        while_blocked.unwrap().changes,
        vec![created("e/link.txt", File)]
    );
    assert!(snapshot.entries.contains_key(Path::new("x/d/g.txt")));
}

#[cfg(target_os = "macos")]
#[test]
fn a_file_moved_from_a_renamed_folder_into_a_folder_the_scan_may_not_read_stays_in_the_renamed_folder()
 {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir(root.join("e")).unwrap();
    fs::write(root.join("a/g.txt"), "g").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(root.join("a"), root.join("b")).unwrap();
    fs::rename(root.join("b/g.txt"), root.join("e/g.txt")).unwrap();
    fs::set_permissions(root.join("e"), fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = blocks_reads(
        "a_file_moved_from_a_renamed_folder_into_a_folder_the_scan_may_not_read_stays_in_the_renamed_folder",
        fs::read_dir(root.join("e")),
    );
    let while_blocked =
        blocked.then(|| ["a", "b"].map(|scope| snapshot.rescan(&root, [PathBuf::from(scope)])));
    fs::set_permissions(root.join("e"), fs::Permissions::from_mode(0o755)).unwrap();
    let Some([renaming, after]) = while_blocked else {
        return;
    };
    let (renaming, after) = (renaming.unwrap(), after.unwrap());
    assert_eq!(renaming.changes, vec![renamed("a", "b", Dir)]);
    assert_eq!(renaming.unsettled, vec![PathBuf::from("b/g.txt")]);
    assert_eq!(after.changes, vec![]);
    assert_eq!(after.unsettled, vec![PathBuf::from("b/g.txt")]);

    let rescan = snapshot.rescan(&root, [PathBuf::from("e")]).unwrap();
    assert_eq!(rescan.changes, vec![renamed("b/g.txt", "e/g.txt", File)]);
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap());
}

#[test]
fn a_file_moved_back_into_the_unreadable_folder_its_folder_left_is_not_removed() {
    let (_dir, root) = moved_and_removed();
    fs::create_dir(root.join("x/d/sub")).unwrap();
    fs::write(root.join("x/d/sub/s.txt"), "s").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(root.join("x/d/sub"), root.join("e/sub")).unwrap();
    fs::rename(root.join("e/sub/s.txt"), root.join("x/d/s.txt")).unwrap();
    fs::set_permissions(root.join("x/d"), fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = blocks_reads(
        "a_file_moved_back_into_the_unreadable_folder_its_folder_left_is_not_removed",
        fs::read_dir(root.join("x/d")),
    );
    let while_blocked = blocked.then(|| snapshot.rescan(&root, [PathBuf::new()]));
    fs::set_permissions(root.join("x/d"), fs::Permissions::from_mode(0o755)).unwrap();
    let Some(while_blocked) = while_blocked else {
        return;
    };
    let changes = while_blocked.unwrap().changes;
    assert!(
        !changes
            .iter()
            .any(|change| matches!(change, Change::Removed { .. })),
        "{changes:?}"
    );
    assert!(
        snapshot.entries.contains_key(Path::new("x/d/sub/s.txt")),
        "{changes:?}"
    );
}

fn moved_into_a_folder_that_turns_unreadable(into: &str) -> (tempfile::TempDir, PathBuf, Snapshot) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::create_dir_all(root.join(into)).unwrap();
    fs::write(root.join("report.docx"), b"report").unwrap();
    let snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(
        root.join("report.docx"),
        root.join(into).join("report.docx"),
    )
    .unwrap();
    fs::set_permissions(root.join("box"), fs::Permissions::from_mode(0o000)).unwrap();
    (dir, root, snapshot)
}

#[test]
fn a_file_moved_into_a_folder_that_turns_unreadable_keeps_its_place_until_the_folder_is_read() {
    for into in ["box", "box/inner"] {
        let (_dir, root, mut snapshot) = moved_into_a_folder_that_turns_unreadable(into);
        let blocked = blocks_reads(
            "a_file_moved_into_a_folder_that_turns_unreadable_keeps_its_place_until_the_folder_is_read",
            fs::read_dir(root.join("box")),
        );
        let while_blocked = blocked.then(|| {
            [PathBuf::new(), PathBuf::from("report.docx")]
                .map(|scope| snapshot.rescan(&root, [scope]))
        });
        fs::set_permissions(root.join("box"), fs::Permissions::from_mode(0o755)).unwrap();
        let Some(while_blocked) = while_blocked else {
            return;
        };
        for rescan in while_blocked {
            let rescan = rescan.unwrap();
            assert_eq!(rescan.changes, vec![], "{into}");
            assert_eq!(
                rescan.unsettled,
                vec![PathBuf::from("report.docx")],
                "{into}"
            );
        }

        let rescan = snapshot.rescan(&root, [PathBuf::from("box")]).unwrap();
        let to = format!("{into}/report.docx");
        assert_eq!(
            rescan.changes,
            vec![renamed("report.docx", &to, File)],
            "{into}"
        );
        assert_eq!(snapshot, Snapshot::scan(&root).unwrap(), "{into}");
    }
}

#[test]
fn a_file_removed_while_a_folder_stays_unreadable_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::create_dir(root.join("box")).unwrap();
    fs::write(root.join("report.docx"), b"report").unwrap();
    fs::set_permissions(root.join("box"), fs::Permissions::from_mode(0o000)).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let scanned = Snapshot::scan(&root).unwrap();
    fs::remove_file(root.join("report.docx")).unwrap();
    let blocked = blocks_reads(
        "a_file_removed_while_a_folder_stays_unreadable_is_removed",
        fs::read_dir(root.join("box")),
    );
    let rescans = blocked.then(|| {
        [scanned.clone(), restarted(&scanned)]
            .map(|mut snapshot| snapshot.rescan(&root, [PathBuf::new()]))
    });
    fs::set_permissions(root.join("box"), fs::Permissions::from_mode(0o755)).unwrap();
    let Some(rescans) = rescans else {
        return;
    };
    for rescan in rescans {
        assert_eq!(rescan.unwrap().changes, vec![removed("report.docx", File)]);
    }
}

fn restarted(snapshot: &Snapshot) -> Snapshot {
    Snapshot {
        entries: snapshot.entries.clone(),
        unread: (snapshot.unread.as_ref()).map(|unread| {
            (unread.iter())
                .filter(|(_, unread)| unread.waiting.is_empty() && unread.ctime_ns.is_some())
                .map(|(dir, unread)| {
                    let ctime_ns = unread.ctime_ns;
                    (
                        dir.clone(),
                        Unread {
                            ctime_ns,
                            ..Unread::default()
                        },
                    )
                })
                .collect()
        }),
        ..Snapshot::default()
    }
}

#[test]
fn a_file_moved_into_a_folder_that_turns_unreadable_keeps_its_place_across_a_restart() {
    let (_dir, root, mut snapshot) = moved_into_a_folder_that_turns_unreadable("box");
    let blocked = blocks_reads(
        "a_file_moved_into_a_folder_that_turns_unreadable_keeps_its_place_across_a_restart",
        fs::read_dir(root.join("box")),
    );
    let while_blocked = blocked.then(|| {
        let first = snapshot.rescan(&root, [PathBuf::new()]);
        std::thread::sleep(Duration::from_millis(50));
        let again = snapshot.rescan(&root, [PathBuf::new()]);
        let mut restarted = restarted(&snapshot);
        let after = restarted.rescan(&root, [PathBuf::new()]);
        (restarted, [first, again, after])
    });
    fs::set_permissions(root.join("box"), fs::Permissions::from_mode(0o755)).unwrap();
    let Some((mut restarted, rescans)) = while_blocked else {
        return;
    };
    for rescan in rescans {
        assert_eq!(rescan.unwrap().changes, vec![]);
    }
    let rescan = restarted.rescan(&root, [PathBuf::from("box")]).unwrap();
    assert_eq!(
        rescan.changes,
        vec![renamed("report.docx", "box/report.docx", File)]
    );
}
fn moved_into_a_settled_unreadable_folder(test: &str, then: &[&str]) -> Option<String> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    for folder in ["box", "src"] {
        fs::create_dir(root.join(folder)).unwrap();
    }
    fs::write(root.join("src/report.docx"), b"report").unwrap();
    fs::write(root.join("notes.txt"), b"notes").unwrap();
    let lock =
        |mode| fs::set_permissions(root.join("box"), fs::Permissions::from_mode(mode)).unwrap();
    lock(0o000);
    let blocked = blocks_reads(test, fs::read_dir(root.join("box")));
    std::thread::sleep(Duration::from_millis(50));
    let mut snapshot = Snapshot::scan(&root).unwrap();
    lock(0o755);
    if !blocked {
        return None;
    }
    fs::rename(root.join("src/report.docx"), root.join("box/report.docx")).unwrap();
    lock(0o000);
    std::thread::sleep(Duration::from_millis(50));
    let mut seen = Vec::new();
    for step in then {
        std::thread::sleep(Duration::from_millis(50));
        match step.split_once(' ') {
            Some(("rename", to)) => fs::rename(root.join("src"), root.join(to)).unwrap(),
            Some(("remove", _)) => fs::remove_dir(root.join("src")).unwrap(),
            Some(("create", _)) => fs::create_dir(root.join("src")).unwrap(),
            Some(("deny", _)) => {
                fs::set_permissions(root.join("src"), fs::Permissions::from_mode(0o000)).unwrap()
            }
            Some(("allow", _)) => {
                fs::set_permissions(root.join("src"), fs::Permissions::from_mode(0o755)).unwrap()
            }
            _ => seen.push(snapshot.rescan(&root, [PathBuf::from(step)])),
        }
    }
    lock(0o755);
    let read = snapshot.rescan(&root, [PathBuf::from("box")]);
    let seen: Vec<Vec<Change>> = (seen.into_iter().chain([read]))
        .map(|rescan| rescan.unwrap().changes)
        .collect();
    let (read, blocked) = seen.split_last().unwrap();
    let removes_the_report = |changes: &Vec<Change>| {
        (changes.iter()).any(|change| matches!(change, Change::Removed { path, .. } if path.ends_with("report.docx")))
    };
    let renamed = (read.iter()).any(
        |change| matches!(change, Change::Renamed { to, .. } if to == Path::new("box/report.docx")),
    );
    let settled = snapshot == Snapshot::scan(&root).unwrap();
    (blocked.iter().any(removes_the_report) || !renamed || !settled)
        .then(|| format!("{test}: settled {settled}, {seen:?}"))
}

#[test]
fn a_move_into_an_unreadable_folder_that_a_scan_elsewhere_saw_first_keeps_its_place() {
    let failed =
        moved_into_a_settled_unreadable_folder("a scan elsewhere first", &["notes.txt", "src", ""]);
    assert_eq!(failed, None);
}

#[test]
fn a_file_waiting_on_an_unreadable_folder_keeps_waiting_when_its_old_folder_is_renamed_or_removed()
{
    let failed: Vec<String> = [
        ("old folder renamed", &["", "rename dst", "", ""][..]),
        ("old folder removed", &["", "remove src", "", ""]),
        (
            "old folder made again",
            &["", "remove src", "", "create src", "", ""],
        ),
        (
            "old folder renamed before a narrow scan",
            &["rename dst", "notes.txt", "", ""],
        ),
        (
            "old folder unreadable for a scan",
            &["", "deny src", "", "allow src", "", ""],
        ),
    ]
    .into_iter()
    .filter_map(|(test, then)| moved_into_a_settled_unreadable_folder(test, then))
    .collect();
    assert_eq!(failed, Vec::<String>::new());
}

#[test]
fn a_file_waiting_on_an_unreadable_folder_is_removed_once_a_file_takes_the_folder_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    for folder in ["root/box", "root/src", "away"] {
        fs::create_dir_all(dir.path().join(folder)).unwrap();
    }
    fs::write(root.join("src/report.docx"), b"report").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(root.join("src/report.docx"), root.join("box/report.docx")).unwrap();
    fs::set_permissions(root.join("box"), fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = blocks_reads(
        "a file in the folder's place",
        fs::read_dir(root.join("box")),
    );
    std::thread::sleep(Duration::from_millis(50));
    let held = snapshot.rescan(&root, [PathBuf::new()]).unwrap().changes;
    fs::set_permissions(root.join("box"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(root.join("box"), dir.path().join("away/box")).unwrap();
    if !blocked {
        return;
    }
    fs::write(root.join("box"), b"box").unwrap();
    let read = snapshot
        .rescan(&root, [PathBuf::from("src/report.docx")])
        .unwrap()
        .changes;
    let removed = (held.iter().chain(&read)).any(
        |change| matches!(change, Change::Removed { path, .. } if path.ends_with("report.docx")),
    );
    assert!(removed, "{held:?} {read:?}");
    snapshot.rescan(&root, [PathBuf::new()]).unwrap();
    assert_eq!(snapshot, Snapshot::scan(&root).unwrap());
}

#[test]
fn a_delete_made_across_an_upgrade_next_to_a_folder_that_stays_unreadable_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::create_dir(root.join("box")).unwrap();
    fs::write(root.join("report.docx"), b"report").unwrap();
    fs::write(root.join("notes.txt"), b"notes").unwrap();
    let lock =
        |mode| fs::set_permissions(root.join("box"), fs::Permissions::from_mode(mode)).unwrap();
    lock(0o000);
    let blocked = blocks_reads("an upgrade", fs::read_dir(root.join("box")));
    std::thread::sleep(Duration::from_millis(50));
    let scanned = Snapshot::scan(&root).unwrap();
    fs::remove_file(root.join("report.docx")).unwrap();
    let mut upgraded = Snapshot {
        entries: scanned.entries.clone(),
        ..Snapshot::default()
    };
    let first = upgraded.rescan(&root, [PathBuf::new()]).unwrap().changes;
    let mut snapshot = restarted(&upgraded);
    let after_a_restart = snapshot.rescan(&root, [PathBuf::new()]).unwrap().changes;
    lock(0o755);
    fs::rename(root.join("notes.txt"), root.join("box/notes.txt")).unwrap();
    lock(0o000);
    std::thread::sleep(Duration::from_millis(50));
    let moved_in = snapshot.rescan(&root, [PathBuf::new()]).unwrap().changes;
    lock(0o755);
    if blocked {
        assert_eq!(first, vec![removed("report.docx", File)]);
        assert_eq!(after_a_restart, vec![]);
        assert_eq!(moved_in, vec![]);
    }
}

#[test]
fn a_file_waiting_on_an_unreadable_folder_keeps_its_place_when_a_narrow_scan_cannot_find_the_folder()
 {
    let failed: Vec<String> = [
        "renamed",
        "renamed and replaced by a file",
        "parent made unreadable",
        "moved out of the tree",
    ]
    .into_iter()
    .filter_map(waiting_folder_out_of_reach)
    .collect();
    assert_eq!(failed, Vec::<String>::new());
}

fn waiting_folder_out_of_reach(case: &str) -> Option<String> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let folder = match case {
        "parent made unreadable" => PathBuf::from("p/box"),
        _ => PathBuf::from("box"),
    };
    fs::create_dir_all(root.join(&folder)).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/report.docx"), b"report").unwrap();
    let mode =
        |path: &Path, mode| fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    mode(&root.join(&folder), 0o000);
    let blocked = blocks_reads(case, fs::read_dir(root.join(&folder)));
    std::thread::sleep(Duration::from_millis(50));
    let mut snapshot = Snapshot::scan(&root).unwrap();
    mode(&root.join(&folder), 0o755);
    if !blocked {
        return None;
    }
    fs::rename(
        root.join("src/report.docx"),
        root.join(&folder).join("report.docx"),
    )
    .unwrap();
    mode(&root.join(&folder), 0o000);
    std::thread::sleep(Duration::from_millis(50));
    let held = match case {
        "parent made unreadable" => Vec::new(),
        _ => snapshot.rescan(&root, [PathBuf::new()]).unwrap().changes,
    };
    mode(&root.join(&folder), 0o755);
    let place = match case {
        "parent made unreadable" => {
            mode(&root.join(&folder), 0o000);
            mode(&root.join("p"), 0o000);
            Some(folder.join("report.docx"))
        }
        "moved out of the tree" => {
            fs::rename(root.join(&folder), dir.path().join("box")).unwrap();
            None
        }
        _ => {
            fs::rename(root.join(&folder), root.join("box2")).unwrap();
            mode(&root.join("box2"), 0o000);
            if case.ends_with("a file") {
                fs::write(root.join(&folder), b"box").unwrap();
            }
            Some(PathBuf::from("box2/report.docx"))
        }
    };
    std::thread::sleep(Duration::from_millis(50));
    let narrow = snapshot
        .rescan(&root, [PathBuf::from("src/report.docx")])
        .unwrap()
        .changes;
    for path in ["p", "p/box", "box2"] {
        let _ = fs::set_permissions(root.join(path), fs::Permissions::from_mode(0o755));
    }
    let read = snapshot.rescan(&root, [PathBuf::new()]).unwrap().changes;
    let removed = |changes: &[Change]| {
        (changes.iter()).any(|change| matches!(change, Change::Removed { path, .. } if path.ends_with("report.docx")))
    };
    let placed = match &place {
        Some(place) => {
            !removed(&narrow)
                && (read.iter())
                    .any(|change| matches!(change, Change::Renamed { to, .. } if to == place))
        }
        None => removed(&narrow) || removed(&read),
    };
    let settled = snapshot == Snapshot::scan(&root).unwrap();
    (removed(&held) || !placed || !settled)
        .then(|| format!("{case}: settled {settled}, {held:?} {narrow:?} {read:?}"))
}
