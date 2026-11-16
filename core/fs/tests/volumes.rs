// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use koidrive_fs::{Change, EntryKind, NativeWatcher, Snapshot, Watcher};
use koidrive_test_support::Disks;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn rescan(snapshot: &mut Snapshot, root: &Path, paths: &[&str]) -> koidrive_fs::Rescan {
    snapshot
        .rescan(root, paths.iter().map(PathBuf::from))
        .unwrap()
}

fn removals(changes: &[Change]) -> Vec<&Change> {
    (changes.iter())
        .filter(|change| matches!(change, Change::Removed { .. } | Change::Renamed { .. }))
        .collect()
}

fn holds(snapshot: &Snapshot, paths: &[&str]) {
    for path in paths {
        assert!(
            snapshot.entries.contains_key(Path::new(path)),
            "the snapshot lost {path}: {:?}",
            snapshot.entries.keys().collect::<Vec<_>>()
        );
    }
}

const VOLUME_FILES: [&str; 2] = ["report.docx", "Slides/deck.key"];

fn fill(at: &Path) {
    for name in VOLUME_FILES {
        write(&at.join(name), name);
    }
}

fn below(mount: &str) -> Vec<String> {
    VOLUME_FILES
        .iter()
        .map(|name| format!("{mount}/{name}"))
        .collect()
}

#[test]
fn a_volume_ejected_from_inside_the_folder_keeps_everything_it_held() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let (outer, inner) = (disks.image("Outer"), disks.image("Inner"));
    write(&root.join("top.txt"), "top");
    fs::create_dir_all(root.join("Disk")).unwrap();
    fs::create_dir_all(root.join("a/b/Deep")).unwrap();
    disks.attach(&outer, &root.join("Disk"));
    disks.attach(&inner, &root.join("a/b/Deep"));
    fill(&root.join("Disk"));
    fill(&root.join("a/b/Deep"));
    let mut snapshot = Snapshot::scan(&root).unwrap();
    let mounts: Vec<&str> = vec!["Disk", "a/b/Deep"];
    let held: Vec<String> = mounts.iter().flat_map(|mount| below(mount)).collect();
    let held: Vec<&str> = held.iter().map(String::as_str).collect();
    for mount in &mounts {
        assert!(snapshot.entries[Path::new(mount)].mount_point, "{mount}");
    }
    assert!(!snapshot.entries[Path::new("a/b")].mount_point);
    holds(&snapshot, &held);

    disks.detach(&root.join("Disk"));
    disks.detach(&root.join("a/b/Deep"));
    for paths in [
        &[""][..],
        &["Disk", "a/b/Deep"],
        &["Disk/Slides", "a/b/Deep/report.docx"],
    ] {
        let found = rescan(&mut snapshot, &root, paths);
        assert_eq!(found.changes, [], "{paths:?}");
        holds(&snapshot, &held);
    }
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(
        found.unreadable.iter().collect::<BTreeSet<_>>(),
        mounts
            .iter()
            .map(Path::new)
            .map(Path::to_path_buf)
            .collect::<BTreeSet<_>>()
            .iter()
            .collect()
    );

    fs::remove_dir(root.join("Disk")).unwrap();
    fs::remove_dir_all(root.join("a/b")).unwrap();
    fs::create_dir(root.join("a/b")).unwrap();
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(removals(&found.changes), Vec::<&Change>::new());
    holds(&snapshot, &held);

    write(&root.join("top.txt"), "edited while the disks are away");
    let found = rescan(&mut snapshot, &root, &["top.txt"]);
    assert_eq!(
        found.changes,
        [Change::Modified {
            path: "top.txt".into()
        }]
    );

    fs::create_dir(root.join("Disk")).unwrap();
    fs::create_dir(root.join("a/b/Deep")).unwrap();
    disks.attach(&outer, &root.join("Disk"));
    disks.attach(&inner, &root.join("a/b/Deep"));
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(found.changes, []);
    assert_eq!(found.unreadable, Vec::<PathBuf>::new());
    write(&root.join("Disk/new.txt"), "new");
    fs::remove_file(root.join("a/b/Deep/report.docx")).unwrap();
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(
        found.changes,
        [
            Change::Removed {
                path: "a/b/Deep/report.docx".into(),
                kind: EntryKind::File
            },
            Change::Created {
                path: "Disk/new.txt".into(),
                kind: EntryKind::File
            },
        ]
    );
}

#[test]
fn another_volume_at_a_mount_point_leaves_the_first_one_held() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let (first, second) = (disks.image("First"), disks.image("Second"));
    fs::create_dir_all(root.join("Disk")).unwrap();
    disks.attach(&first, &root.join("Disk"));
    fill(&root.join("Disk"));
    let mut snapshot = Snapshot::scan(&root).unwrap();
    disks.detach(&root.join("Disk"));
    disks.attach(&second, &root.join("Disk"));
    write(&root.join("Disk/other.txt"), "other");
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(found.changes, []);
    assert_eq!(found.unreadable, [PathBuf::from("Disk")]);
    holds(
        &snapshot,
        &below("Disk").iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert!(!snapshot.entries.contains_key(Path::new("Disk/other.txt")));
}

fn clones(disks: &mut Disks) -> (PathBuf, PathBuf) {
    let one = disks.image("One");
    let staging = disks.base().join("staging");
    fs::create_dir(&staging).unwrap();
    disks.attach(&one, &staging);
    for name in ["S/f.txt", "S/g.txt"] {
        write(&staging.join(name), name);
    }
    disks.detach(&staging);
    let two = disks.copy(&one, "Two");
    (one, two)
}

#[test]
fn renames_pair_entries_of_one_volume_only_also_on_byte_identical_copies() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let (one, two) = clones(&mut disks);
    fs::create_dir_all(root.join("A")).unwrap();
    disks.attach(&one, &root.join("A"));
    let mut snapshot = Snapshot::scan(&root).unwrap();

    fs::remove_file(root.join("A/S/f.txt")).unwrap();
    fs::create_dir(root.join("B")).unwrap();
    disks.attach(&two, &root.join("B"));
    let found = rescan(&mut snapshot, &root, &[""]);
    assert!(
        found.changes.contains(&Change::Removed {
            path: "A/S/f.txt".into(),
            kind: EntryKind::File
        }),
        "{:?}",
        found.changes
    );
    assert!(
        !found
            .changes
            .iter()
            .any(|change| matches!(change, Change::Renamed { .. })),
        "{:?}",
        found.changes
    );
    assert!(snapshot.entries[Path::new("B")].mount_point);

    fs::remove_file(root.join("A/S/g.txt")).unwrap();
    fs::rename(root.join("B/S/g.txt"), root.join("B/S/h.txt")).unwrap();
    let found = rescan(&mut snapshot, &root, &["A/S", "B/S"]);
    assert_eq!(
        found.changes,
        [
            Change::Removed {
                path: "A/S/g.txt".into(),
                kind: EntryKind::File
            },
            Change::Renamed {
                from: "B/S/g.txt".into(),
                to: "B/S/h.txt".into(),
                kind: EntryKind::File
            },
        ]
    );
}

#[test]
fn a_folder_above_a_mounted_volume_moves_with_the_volume_in_it() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let disk = disks.image("Moving");
    fs::create_dir_all(root.join("p/q/m")).unwrap();
    disks.attach(&disk, &root.join("p/q/m"));
    fill(&root.join("p/q/m"));
    let mut snapshot = Snapshot::scan(&root).unwrap();
    for (from, to) in [("p", "p2"), ("p2", "p")] {
        fs::rename(root.join(from), root.join(to)).unwrap();
        disks.moved(&root.join(from), &root.join(to));
        let found = rescan(&mut snapshot, &root, &[""]);
        assert_eq!(
            found.changes,
            [Change::Renamed {
                from: from.into(),
                to: to.into(),
                kind: EntryKind::Dir
            }]
        );
        assert!(snapshot.entries[&Path::new(to).join("q/m")].mount_point);
        holds(
            &snapshot,
            &below(&format!("{to}/q/m"))
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
    }

    disks.detach(&root.join("p/q/m"));
    fs::rename(root.join("p"), root.join("p3")).unwrap();
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(
        found.changes,
        [Change::Renamed {
            from: "p".into(),
            to: "p3".into(),
            kind: EntryKind::Dir
        }]
    );
    holds(
        &snapshot,
        &below("p3/q/m")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn the_watcher_takes_a_volume_back_when_it_is_mounted_again() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let disk = disks.image("Back");
    fs::create_dir_all(root.join("Disk")).unwrap();
    disks.attach(&disk, &root.join("Disk"));
    fill(&root.join("Disk"));
    let mut watcher = NativeWatcher::start(&root, Snapshot::default()).unwrap();
    let mut created = 0;
    let started = Instant::now();
    while created < 4 {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the first scan found {created} entries"
        );
        created += watcher
            .poll(Duration::from_millis(200))
            .unwrap()
            .changes
            .len();
    }
    disks.detach(&root.join("Disk"));
    let mut unreadable = Vec::new();
    let started = Instant::now();
    while !unreadable.contains(&PathBuf::from("Disk")) {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the ejected disk was never held"
        );
        let poll = watcher.poll(Duration::from_millis(200)).unwrap();
        assert_eq!(removals(&poll.changes), Vec::<&Change>::new());
        unreadable.extend(poll.unreadable);
    }
    disks.attach(&disk, &root.join("Disk"));
    write(&root.join("Disk/after.txt"), "after");
    let started = Instant::now();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the disk mounted again was never read"
        );
        let poll = watcher.poll(Duration::from_millis(200)).unwrap();
        assert_eq!(removals(&poll.changes), Vec::<&Change>::new());
        if poll.changes.contains(&Change::Created {
            path: "Disk/after.txt".into(),
            kind: EntryKind::File,
        }) {
            break;
        }
    }
}
