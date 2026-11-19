// SPDX-License-Identifier: Apache-2.0

mod common;

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{bind, rewrite_keeping_mtime, same};
use koidrive_fs::{
    Base, BroughtBack, Location, PRIVATE_DIR, Reclaimed, Space, TRASH_DIR, TRASH_RETENTION,
    recovered_name,
};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const T0: u64 = 1_700_000_000;

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn trash_dir(root: &Path) -> PathBuf {
    root.join(PRIVATE_DIR).join(TRASH_DIR)
}

fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn put(root: &Path, space: &Space, name: &str, entry: &str, now: SystemTime) {
    let path = root.join(name);
    let content = (!path.is_dir()).then(|| fs::read(&path).unwrap());
    let location = Location::At {
        base: Base::Root,
        dirs: Vec::new(),
        name: name.into(),
    };
    let (folder, name) = space.parent(&location).unwrap();
    space
        .trash(&folder, name, entry, name, content.as_deref(), now)
        .unwrap();
}

fn removed(entries: &[&str]) -> Reclaimed {
    Reclaimed {
        removed: entries.iter().map(|entry| entry.to_string()).collect(),
        recovered: Vec::new(),
    }
}

#[test]
fn trashed_entries_stay_until_they_expire() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("old.txt"), "old").unwrap();
    fs::write(root.join("new.txt"), "new").unwrap();
    let space = bind(root);
    put(root, &space, "old.txt", "e1", at(T0));
    put(root, &space, "new.txt", "e2", at(T0) + 3 * DAY);
    assert_eq!(names(root), [PRIVATE_DIR]);
    assert_eq!(names(&trash_dir(root)), ["e1", "e2"]);

    let before_expiry = at(T0) + TRASH_RETENTION - Duration::from_secs(1);
    assert_eq!(
        space
            .reclaim_trash(before_expiry, u64::MAX, &mut same)
            .unwrap(),
        Reclaimed::default()
    );
    assert_eq!(
        space
            .reclaim_trash(at(T0) + TRASH_RETENTION, u64::MAX, &mut same)
            .unwrap(),
        removed(&["e1"])
    );
    assert_eq!(names(&trash_dir(root)), ["e2"]);
    assert_eq!(fs::read(trash_dir(root).join("e2")).unwrap(), b"new");
    assert_eq!(
        space
            .reclaim_trash(at(T0) + 3 * DAY + TRASH_RETENTION, u64::MAX, &mut same)
            .unwrap(),
        removed(&["e2"])
    );
    assert!(names(&trash_dir(root)).is_empty());
}

#[test]
fn size_cap_evicts_the_oldest_entries_first() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let space = bind(root);
    for (i, size) in [10, 20, 30].into_iter().enumerate() {
        let name = format!("f{i}");
        fs::write(root.join(&name), vec![b'x'; size]).unwrap();
        put(root, &space, &name, &format!("e{i}"), at(T0 + i as u64));
    }
    fs::create_dir(root.join("empty")).unwrap();
    put(root, &space, "empty", "e3", at(T0 + 3));

    assert_eq!(
        space.reclaim_trash(at(T0 + 4), 60, &mut same).unwrap(),
        Reclaimed::default()
    );
    assert_eq!(
        space.reclaim_trash(at(T0 + 4), 35, &mut same).unwrap(),
        removed(&["e0", "e1"])
    );
    assert_eq!(names(&trash_dir(root)), ["e2", "e3"]);
    assert_eq!(
        space.reclaim_trash(at(T0 + 4), 0, &mut same).unwrap(),
        removed(&["e2"])
    );
    assert_eq!(names(&trash_dir(root)), ["e3"]);
    assert_eq!(
        space
            .reclaim_trash(at(T0 + 3) + TRASH_RETENTION, 0, &mut same)
            .unwrap(),
        removed(&["e3"])
    );
    assert!(names(&trash_dir(root)).is_empty());
}

#[test]
fn write_through_an_old_descriptor_is_recovered_on_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("report.txt"), "synced").unwrap();
    let mut held = OpenOptions::new()
        .append(true)
        .open(root.join("report.txt"))
        .unwrap();
    let space = bind(root);
    put(root, &space, "report.txt", "e1", at(T0));

    held.write_all(b" and appended later").unwrap();
    drop(held);
    let reclaimed = space
        .reclaim_trash(at(T0) + TRASH_RETENTION, u64::MAX, &mut same)
        .unwrap();
    let name = "report (recovered 2023-11-21).txt";
    assert_eq!(
        reclaimed,
        Reclaimed {
            removed: Vec::new(),
            recovered: vec![name.into()],
        }
    );
    assert_eq!(
        fs::read_to_string(root.join(name)).unwrap(),
        "synced and appended later"
    );
    assert!(names(&trash_dir(root)).is_empty());
}

#[test]
fn write_through_an_old_descriptor_is_recovered_under_the_size_cap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("notes"), "0123456789").unwrap();
    fs::write(root.join("other.txt"), "other").unwrap();
    let held = OpenOptions::new()
        .write(true)
        .open(root.join("notes"))
        .unwrap();
    let space = bind(root);
    put(root, &space, "notes", "e1", at(T0));
    fs::write(root.join("notes (recovered 2023-11-14)"), "taken").unwrap();

    std::thread::sleep(Duration::from_millis(50));
    held.write_all_at(b"ABCDEFGHIJ", 0).unwrap();
    drop(held);
    put(root, &space, "other.txt", "e2", at(T0 + 1));
    let reclaimed = space.reclaim_trash(at(T0 + 2), 5, &mut same).unwrap();
    let name = "notes (recovered 2023-11-14 2)";
    assert_eq!(
        reclaimed,
        Reclaimed {
            removed: Vec::new(),
            recovered: vec![name.into()],
        }
    );
    assert_eq!(fs::read_to_string(root.join(name)).unwrap(), "ABCDEFGHIJ");
    assert_eq!(
        fs::read_to_string(root.join("notes (recovered 2023-11-14)")).unwrap(),
        "taken"
    );
    assert_eq!(names(&trash_dir(root)), ["e2"]);
}

#[test]
fn an_entry_whose_metadata_alone_changed_is_deleted_when_due() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("report.txt"), "synced").unwrap();
    fs::create_dir(root.join("empty.d")).unwrap();
    let space = bind(root);
    put(root, &space, "report.txt", "e1", at(T0));
    put(root, &space, "empty.d", "e2", at(T0));

    std::thread::sleep(Duration::from_millis(50));
    for entry in ["e1", "e2"] {
        let path = trash_dir(root).join(entry);
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }
    assert_eq!(
        space
            .reclaim_trash(at(T0) + TRASH_RETENTION, u64::MAX, &mut same)
            .unwrap(),
        removed(&["e1", "e2"])
    );
    assert_eq!(names(root), [PRIVATE_DIR]);
}

#[test]
fn a_write_that_keeps_size_and_modification_time_is_recovered_when_due() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("report.txt"), "synced").unwrap();
    let space = bind(root);
    put(root, &space, "report.txt", "e1", at(T0));

    std::thread::sleep(Duration::from_millis(50));
    rewrite_keeping_mtime(&trash_dir(root).join("e1"), b"SYNCED");
    let reclaimed = space
        .reclaim_trash(at(T0) + TRASH_RETENTION, u64::MAX, &mut same)
        .unwrap();
    assert_eq!(reclaimed.removed, Vec::<String>::new());
    assert_eq!(reclaimed.recovered.len(), 1);
    assert_eq!(
        fs::read(root.join(&reclaimed.recovered[0])).unwrap(),
        b"SYNCED"
    );
}

#[test]
fn an_unreadable_entry_is_recovered_and_the_pass_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("locked.txt"), "synced").unwrap();
    fs::write(root.join("report.txt"), "synced").unwrap();
    let space = bind(root);
    put(root, &space, "locked.txt", "e1", at(T0));
    put(root, &space, "report.txt", "e2", at(T0 + 1));
    let locked = trash_dir(root).join("e1");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&locked).is_ok() {
        eprintln!("skipped: the entry is still readable, as for root");
        return;
    }

    let reclaimed = space
        .reclaim_trash(at(T0 + 1) + TRASH_RETENTION, u64::MAX, &mut same)
        .unwrap();
    assert_eq!(reclaimed.removed, ["e2"]);
    assert_eq!(reclaimed.recovered.len(), 1, "{reclaimed:?}");
    assert_eq!(names(&trash_dir(root)), Vec::<String>::new());
}

#[test]
fn folder_written_into_after_trashing_is_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("photos.d")).unwrap();
    let held = fs::File::open(root.join("photos.d")).unwrap();
    let space = bind(root);
    put(root, &space, "photos.d", "e1", at(T0));

    std::thread::sleep(Duration::from_millis(50));
    let inside = rustix::fs::openat(
        &held,
        "late.txt",
        rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE,
        rustix::fs::Mode::from_raw_mode(0o644),
    )
    .unwrap();
    fs::File::from(inside).write_all(b"late").unwrap();
    let reclaimed = space
        .reclaim_trash(at(T0) + TRASH_RETENTION, u64::MAX, &mut same)
        .unwrap();
    let name = "photos.d (recovered 2023-11-21)";
    assert_eq!(reclaimed.recovered, [OsStr::new(name)]);
    assert_eq!(
        fs::read_to_string(root.join(name).join("late.txt")).unwrap(),
        "late"
    );
}

#[test]
fn entry_missing_from_the_index_is_kept_for_a_full_period() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let space = bind(root);
    fs::write(root.join("a.txt"), "a").unwrap();
    put(root, &space, "a.txt", "e1", at(T0));
    fs::write(trash_dir(root).join("e2"), "moved in before a crash").unwrap();
    fs::remove_file(trash_dir(root).join("e1")).unwrap();

    let later = at(T0) + TRASH_RETENTION;
    assert_eq!(
        space.reclaim_trash(later, u64::MAX, &mut same).unwrap(),
        Reclaimed::default()
    );
    assert_eq!(names(&trash_dir(root)), ["e2"]);
    assert_eq!(
        space
            .reclaim_trash(later + TRASH_RETENTION, u64::MAX, &mut same)
            .unwrap(),
        Reclaimed {
            removed: Vec::new(),
            recovered: vec!["e2 (recovered 2023-11-28)".into()],
        }
    );
}

#[test]
fn trashing_never_replaces_an_entry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let space = bind(root);
    fs::write(root.join("a.txt"), "a").unwrap();
    fs::write(root.join("b.txt"), "b").unwrap();
    put(root, &space, "a.txt", "e1", at(T0));
    let location = Location::At {
        base: Base::Root,
        dirs: Vec::new(),
        name: "b.txt".into(),
    };
    let (folder, name) = space.parent(&location).unwrap();
    let err = space
        .trash(&folder, name, "e1", name, None, at(T0))
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(trash_dir(root).join("e1")).unwrap(), b"a");
    assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"b");
}

#[test]
fn recovered_names_keep_the_extension_and_fit() {
    let date = "2026-11-10";
    let name = |original: &str, folder: bool, n: u32| {
        recovered_name(OsStr::new(original), folder, date, n)
            .into_string()
            .unwrap()
    };
    assert_eq!(
        name("report.txt", false, 1),
        "report (recovered 2026-11-10).txt"
    );
    assert_eq!(
        name("report.txt", false, 3),
        "report (recovered 2026-11-10 3).txt"
    );
    assert_eq!(
        name("archive.tar.gz", false, 1),
        "archive.tar (recovered 2026-11-10).gz"
    );
    assert_eq!(name(".bashrc", false, 1), ".bashrc (recovered 2026-11-10)");
    assert_eq!(
        name("Makefile", false, 1),
        "Makefile (recovered 2026-11-10)"
    );
    assert_eq!(name("photos.d", true, 1), "photos.d (recovered 2026-11-10)");

    let long = format!("{}.txt", "é".repeat(126));
    let fitted = name(&long, false, 12);
    assert!(fitted.len() <= 255, "{}", fitted.len());
    assert!(fitted.starts_with("éé"));
    assert!(fitted.ends_with(" (recovered 2026-11-10 12).txt"));

    let long_extension = format!("x.{}", "e".repeat(251));
    let fitted = name(&long_extension, false, 1);
    assert!(fitted.len() <= 255, "{}", fitted.len());
    assert!(fitted.starts_with("x.eee"));
    assert!(fitted.ends_with(" (recovered 2026-11-10)"));
}

fn put_unindexed(root: &Path, name: &str, entry: &str) {
    fs::create_dir_all(trash_dir(root)).unwrap();
    fs::rename(root.join(name), trash_dir(root).join(entry)).unwrap();
}

#[test]
fn unindexed_entry_written_late_is_recovered_under_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("notes.txt"), "synced").unwrap();
    let mut held = OpenOptions::new()
        .append(true)
        .open(root.join("notes.txt"))
        .unwrap();
    put_unindexed(root, "notes.txt", "e1");
    held.write_all(b" and written late").unwrap();
    drop(held);

    let space = bind(root);
    let reclaimed = space.reclaim_trash(at(T0), 0, &mut same).unwrap();
    let name = "e1 (recovered 2023-11-14)";
    assert_eq!(
        reclaimed,
        Reclaimed {
            removed: Vec::new(),
            recovered: vec![name.into()],
        }
    );
    assert_eq!(
        fs::read_to_string(root.join(name)).unwrap(),
        "synced and written late"
    );
}

#[test]
fn unindexed_entry_written_late_is_recovered_on_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("notes.txt"), "synced").unwrap();
    let mut held = OpenOptions::new()
        .append(true)
        .open(root.join("notes.txt"))
        .unwrap();
    put_unindexed(root, "notes.txt", "e1");
    held.write_all(b" and written late").unwrap();
    drop(held);

    let space = bind(root);
    assert_eq!(
        space.reclaim_trash(at(T0), u64::MAX, &mut same).unwrap(),
        Reclaimed::default()
    );
    let reclaimed = space
        .reclaim_trash(at(T0) + TRASH_RETENTION, u64::MAX, &mut same)
        .unwrap();
    let name = "e1 (recovered 2023-11-21)";
    assert_eq!(
        reclaimed,
        Reclaimed {
            removed: Vec::new(),
            recovered: vec![name.into()],
        }
    );
    assert_eq!(
        fs::read_to_string(root.join(name)).unwrap(),
        "synced and written late"
    );
}

#[test]
fn unindexed_entry_is_never_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.txt"), "a").unwrap();
    put_unindexed(root, "a.txt", "e1");
    let space = bind(root);
    let reclaimed = space
        .reclaim_trash(at(T0) + TRASH_RETENTION, 0, &mut same)
        .unwrap();
    assert_eq!(
        reclaimed,
        Reclaimed {
            removed: Vec::new(),
            recovered: vec!["e1 (recovered 2023-11-21)".into()],
        }
    );
    assert_eq!(
        fs::read(root.join("e1 (recovered 2023-11-21)")).unwrap(),
        b"a"
    );
}

#[test]
fn stale_index_temporary_file_does_not_stop_reclamation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.txt"), "a").unwrap();
    let space = bind(root);
    put(root, &space, "a.txt", "e1", at(T0));
    fs::write(root.join(PRIVATE_DIR).join("trash.json.tmp"), "torn").unwrap();
    fs::write(root.join("b.txt"), "b").unwrap();
    put(root, &space, "b.txt", "e2", at(T0 + 1));

    assert_eq!(
        space.reclaim_trash(at(T0 + 2), 1, &mut same).unwrap(),
        removed(&["e1"])
    );
    assert_eq!(names(&trash_dir(root)), ["e2"]);
}

#[test]
fn trash_names_must_be_single_components() {
    let outside = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    fs::create_dir_all(root.join("d")).unwrap();
    fs::write(root.join("d/a.txt"), "a").unwrap();
    let space = bind(&root);
    let location = Location::At {
        base: Base::Root,
        dirs: Vec::new(),
        name: "d".into(),
    };
    let (folder, _) = space.parent(&location).unwrap();
    let escaped = outside.path().join("escaped");
    let escaped = escaped.to_str().unwrap();
    for (name, entry, original) in [
        ("d", "../../escaped", "a.txt"),
        ("d", escaped, "a.txt"),
        ("d", "e/1", "a.txt"),
        ("d", "..", "a.txt"),
        ("d", ".", "a.txt"),
        ("d", "", "a.txt"),
        ("d/a.txt", "e1", "a.txt"),
        ("../space/d", "e1", "a.txt"),
        ("d", "e1", "../a.txt"),
        ("d", "e1", ".."),
    ] {
        let err = space
            .trash(
                &folder,
                OsStr::new(name),
                entry,
                OsStr::new(original),
                None,
                at(T0),
            )
            .unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "{name:?} {entry:?} {original:?}"
        );
    }
    assert_eq!(names(&root), ["d"]);
    assert_eq!(names(&root.join("d")), ["a.txt"]);
    assert!(names(outside.path()).is_empty());
    assert!(names(dir.path()).iter().all(|name| name == "space"));
}

#[test]
fn private_records_that_are_not_files_are_unreadable_without_waiting_on_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::write(root.join("a.txt"), "a").unwrap();
    put(&root, &bind(&root), "a.txt", "e1", at(T0));
    let private = root.join(PRIVATE_DIR);
    fs::remove_file(private.join("trash.json")).unwrap();
    for record in ["trash.json", "brought-back.json"] {
        koidrive_test_support::make_fifo(&private.join(record));
    }

    let (reclaimed, brought_back) = koidrive_test_support::without_waiting(move || {
        let space = bind(&root);
        let reclaimed = space.reclaim_trash(at(T0) + TRASH_RETENTION, u64::MAX, &mut same);
        (
            reclaimed.map_err(|err| err.kind()),
            space.brought_back().unwrap(),
        )
    });
    assert_eq!(reclaimed.err(), Some(std::io::ErrorKind::InvalidInput));
    assert_eq!(brought_back, BroughtBack::Unreadable);
}

#[test]
fn trash_moved_out_of_the_root_during_reclamation_is_left_alone() {
    let outside = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.txt"), "a").unwrap();
    fs::write(root.join("b.txt"), "b").unwrap();
    let space = bind(root);
    put(root, &space, "a.txt", "e1", at(T0));
    put(root, &space, "b.txt", "e2", at(T0));

    let mut moved = false;
    let mut move_out = |file: &fs::File, content: &[u8]| {
        if !std::mem::replace(&mut moved, true) {
            fs::rename(trash_dir(root), outside.path().join("trash")).unwrap();
        }
        same(file, content)
    };
    let reclaimed = bind(root).reclaim_trash(at(T0) + TRASH_RETENTION, u64::MAX, &mut move_out);

    assert!(reclaimed.is_err());
    assert_eq!(
        fs::read(outside.path().join("trash").join("e2")).unwrap(),
        b"b"
    );
    assert_eq!(names(outside.path()), ["trash"]);
}
