// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use koidrive_fs::{Change, Detached, EntryKind, Mount, NativeWatcher, Snapshot, Watcher};
use koidrive_test_support::{Disks, is_volume_metadata};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn rescan(snapshot: &mut Snapshot, root: &Path, paths: &[&str]) -> koidrive_fs::Rescan {
    let mut found = snapshot
        .rescan(root, paths.iter().map(PathBuf::from))
        .unwrap();
    found
        .changes
        .retain(|change| !paths_of(change).any(is_volume_metadata));
    found
}

fn paths_of(change: &Change) -> impl Iterator<Item = &Path> {
    match change {
        Change::Renamed { from, to, .. } => vec![from.as_path(), to.as_path()],
        Change::Created { path, .. }
        | Change::Removed { path, .. }
        | Change::Modified { path }
        | Change::Reidentified { path } => vec![path.as_path()],
        other => panic!("unexpected change {other:?}"),
    }
    .into_iter()
}

fn mounted(snapshot: &Snapshot, path: &str) -> bool {
    matches!(
        snapshot.entries[Path::new(path)].mount,
        Some(Mount::Volume(_))
    )
}

fn away(snapshot: &Snapshot) -> Vec<&str> {
    (snapshot.detached.iter())
        .filter(|(_, why)| **why == Detached::Away)
        .map(|(path, _)| path.to_str().unwrap())
        .collect()
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

fn permissions_block_reads() -> bool {
    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block reads");
        return false;
    }
    true
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
        assert!(mounted(&snapshot, mount), "{mount}");
    }
    assert_eq!(snapshot.entries[Path::new("a/b")].mount, None);
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
    assert_eq!(away(&snapshot), mounts);

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
    assert_eq!(away(&snapshot), Vec::<&str>::new());
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
    assert_eq!(away(&snapshot), ["Disk"]);
}

#[test]
fn a_volume_mounted_over_a_folder_leaves_what_the_folder_held_until_it_is_ejected() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let disk = disks.image("Over");
    write(&root.join("Data/keep.docx"), "keep");
    let mut snapshot = Snapshot::scan(&root).unwrap();
    disks.attach(&disk, &root.join("Data"));
    write(&root.join("Data/vol.txt"), "on the disk");
    for _ in 0..2 {
        let found = rescan(&mut snapshot, &root, &[""]);
        assert_eq!(found.changes, []);
        assert_eq!(found.unreadable, [PathBuf::from("Data")]);
        holds(&snapshot, &["Data/keep.docx"]);
        assert!(!snapshot.entries.contains_key(Path::new("Data/vol.txt")));
        assert_eq!(
            snapshot.detached.get(Path::new("Data")),
            Some(&Detached::Covered)
        );
    }

    disks.detach(&root.join("Data"));
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(found.changes, []);
    assert!(snapshot.detached.is_empty());
    write(&root.join("Data/keep.docx"), "edited after the eject");
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(
        found.changes,
        [Change::Modified {
            path: "Data/keep.docx".into()
        }]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_volume_without_a_uuid_is_never_read() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let fat = disks.fat_without_serial("Fat");
    fs::create_dir_all(root.join("Disk")).unwrap();
    disks.attach(&fat, &root.join("Disk"));
    fill(&root.join("Disk"));
    let mut snapshot = Snapshot::scan(&root).unwrap();
    assert_eq!(
        snapshot.entries[Path::new("Disk")].mount,
        Some(Mount::Unidentified)
    );
    assert_eq!(away(&snapshot), ["Disk"]);
    for path in below("Disk") {
        assert!(!snapshot.entries.contains_key(Path::new(&path)), "{path}");
    }
    write(&root.join("Disk/later.txt"), "later");
    fs::remove_file(root.join("Disk/report.docx")).unwrap();
    let found = rescan(&mut snapshot, &root, &["", "Disk"]);
    assert_eq!(found.changes, []);
    assert_eq!(found.unreadable, [PathBuf::from("Disk")]);
}

fn identified(at: &Path) -> koidrive_fs::RootAt {
    use std::os::fd::AsFd;
    let started = Instant::now();
    loop {
        let found = koidrive_fs::root_at(fs::File::open(at).unwrap().as_fd()).unwrap();
        if found.volume.is_some() || started.elapsed() > Duration::from_secs(10) {
            return found;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn sticks_mounted_in_turn_at_one_folder_have_volume_ids_of_their_own() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let mut pairs = vec![
        (disks.fat("One"), disks.fat("Two")),
        (
            disks.fat_with_serial("OneOfTwo", 16, 0x0905_0f47),
            disks.fat_with_serial("TwoOfTwo", 32, 0x0905_0f47),
        ),
    ];
    if disks.exfat_has_ids() {
        pairs.push((disks.exfat("OneCard"), disks.exfat("TwoCard")));
        pairs.push((
            disks.exfat_with_serial("OneOfTwoCard", 16, 0x0905_0e7f),
            disks.exfat_with_serial("TwoOfTwoCard", 32, 0x0905_0e7f),
        ));
    }
    let (at, elsewhere) = (disks.base().join("Stick"), disks.base().join("Elsewhere"));
    fs::create_dir_all(&at).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    for (one, two) in &pairs {
        disks.attach(one, &at);
        let first = identified(&at);
        disks.detach(&at);
        disks.attach(two, &at);
        let second = identified(&at);
        disks.attach(one, &elsewhere);
        let again = identified(&elsewhere);
        let still = identified(&at);
        disks.detach(&elsewhere);
        disks.detach(&at);
        let context = format!("{one:?} {first:?} {second:?} {again:?} {still:?}");
        assert!(
            first.volume.is_some() && second.volume.is_some(),
            "{context}"
        );
        assert_ne!(second.volume, first.volume, "{context}");
        assert_eq!(again.volume, first.volume, "{context}");
        assert_eq!(still.volume, second.volume, "{context}");
        assert!(first.admits(&again).is_ok(), "{context}");
        let other = first.admits(&second);
        assert!(
            matches!(other, Err(koidrive_fs::RootProblem::OtherDisk)),
            "{context}"
        );
    }
}

#[cfg(target_os = "linux")]
fn sudo(args: &[&std::ffi::OsStr]) {
    let out = std::process::Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "sudo {args:?}: {out:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn an_exfat_stick_udev_has_no_record_of_is_held_until_it_has() {
    use std::os::unix::fs::MetadataExt;
    let Some(mut disks) = Disks::new() else {
        return;
    };
    if !disks.exfat_has_ids() {
        return;
    }
    let root = disks.base().join("Sync");
    let at = root.join("Disk");
    fs::create_dir_all(&at).unwrap();
    let image = disks.exfat("Card");
    disks.attach(&image, &at);
    assert!(identified(&at).volume.is_some());
    fill(&at);
    let mut snapshot = Snapshot::scan(&root).unwrap();
    let mut expected = snapshot.entries.clone();
    let mount = expected[Path::new("Disk")].mount;
    let dev = fs::metadata(&at).unwrap().dev();
    let (major, minor) = (rustix::fs::major(dev), rustix::fs::minor(dev));
    let record = PathBuf::from(format!("/run/udev/data/b{major}:{minor}"));
    let kept = disks.base().join("record");
    fs::copy(&record, &kept).unwrap();
    let stale = disks.base().join("stale");
    let lines = fs::read_to_string(&kept).unwrap();
    let older = lines
        .lines()
        .filter(|line| !line.starts_with("S:disk/by-diskseq/"));
    fs::write(
        &stale,
        older.map(|line| format!("{line}\n")).collect::<String>(),
    )
    .unwrap();
    for case in ["stale", "missing"] {
        match case {
            "stale" => sudo(&["cp".as_ref(), stale.as_os_str(), record.as_os_str()]),
            _ => sudo(&["rm".as_ref(), "-f".as_ref(), record.as_os_str()]),
        }
        let found = rescan(&mut snapshot, &root, &["", "Disk"]);
        let without = (identified(&at).volume, away(&snapshot) == ["Disk"]);
        sudo(&["cp".as_ref(), kept.as_os_str(), record.as_os_str()]);
        assert_eq!(without, (None, true), "{case}");
        assert_eq!(found.changes, [], "{case}");
        assert_eq!(snapshot.entries, expected, "{case}");
        assert_eq!(snapshot.entries[Path::new("Disk")].mount, mount, "{case}");
        assert!(identified(&at).volume.is_some(), "{case}");
        let back = rescan(&mut snapshot, &root, &[""]).changes;
        let created = (back.iter()).filter(|change| matches!(change, Change::Created { .. }));
        assert_eq!(removals(&back), Vec::<&Change>::new(), "{case}");
        assert_eq!(created.count(), 0, "{case}: {back:?}");
        assert!(away(&snapshot).is_empty(), "{case}");
        expected = snapshot.entries.clone();
    }
    disks.detach(&at);
}

#[cfg(target_os = "linux")]
#[test]
fn a_stick_taken_back_by_a_rescan_of_one_of_its_files_is_recorded_before_the_next_stick() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    for legacy in [true, false] {
        let root = disks.base().join(format!("Sync{legacy}"));
        let at = root.join("Disk");
        fs::create_dir_all(&at).unwrap();
        let first = match legacy {
            true => disks.fat_with_serial(&format!("Recorded{legacy}"), 16, 0x4b4f_1095),
            false => disks.fat_without_serial(&format!("Unknown{legacy}")),
        };
        let (stick, next) = match legacy {
            true => (
                first.clone(),
                disks.fat_with_serial("Bigger", 32, 0x4b4f_1095),
            ),
            false => (
                disks.fat(&format!("Stick{legacy}")),
                disks.fat(&format!("Next{legacy}")),
            ),
        };
        disks.attach(&first, &at);
        fill(&at);
        let mut snapshot = Snapshot::scan(&root).unwrap();
        if legacy {
            let mut plain = [0; 16];
            plain[..4].copy_from_slice(&0x4b4f_1095_u32.to_le_bytes());
            plain[15] = 4;
            let recorded = Some(Mount::Volume(koidrive_fs::VolumeId::Uuid(plain)));
            snapshot.entries.get_mut(Path::new("Disk")).unwrap().mount = recorded;
        }
        disks.detach(&at);
        rescan(&mut snapshot, &root, &[""]);
        disks.attach(&stick, &at);
        identified(&at);
        write(&at.join("photo.jpg"), "photo");
        rescan(&mut snapshot, &root, &["Disk/photo.jpg"]);
        disks.detach(&at);
        disks.attach(&next, &at);
        identified(&at);
        let found = rescan(&mut snapshot, &root, &[""]);
        disks.detach(&at);
        assert_eq!(
            removals(&found.changes),
            Vec::<&Change>::new(),
            "legacy={legacy}"
        );
        assert_eq!(away(&snapshot), ["Disk"], "legacy={legacy}");
        assert!(
            snapshot.entries.contains_key(Path::new("Disk/photo.jpg")),
            "legacy={legacy}"
        );
    }
}

#[test]
fn a_stick_plugged_back_in_comes_back_with_an_edit_made_on_another_computer() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let mut sticks = vec![
        ("FAT", disks.fat("Stick"), true, false),
        ("empty FAT", disks.fat("Empty"), false, false),
        ("edited FAT", disks.fat("Edited"), false, true),
        ("edited disk", disks.image("Drive"), false, true),
    ];
    if disks.exfat_has_ids() {
        sticks.push(("exFAT", disks.exfat("Card"), true, false));
        sticks.push(("edited exFAT", disks.exfat("EditedCard"), false, true));
    }
    for (kind, image, filled, edited) in sticks {
        let root = disks.base().join(kind);
        let (at, elsewhere) = (
            root.join("Disk"),
            disks.base().join(format!("Elsewhere {kind}")),
        );
        fs::create_dir_all(&at).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        disks.attach(&image, &at);
        let first = identified(&at);
        if filled {
            fill(&at);
        }
        if edited {
            write(&at.join("report.docx"), "draft");
        }
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let expected = snapshot.entries.clone();
        disks.detach(&at);
        rescan(&mut snapshot, &root, &["", "Disk"]);
        assert_eq!(away(&snapshot), ["Disk"], "{kind}");
        std::thread::sleep(Duration::from_millis(2100));
        if edited {
            disks.attach(&image, &elsewhere);
            write(
                &elsewhere.join("report.docx"),
                "final version, edited at work",
            );
            disks.detach(&elsewhere);
        }
        disks.attach(&image, &at);
        assert_eq!(identified(&at).volume, first.volume, "{kind}");
        let found = rescan(&mut snapshot, &root, &["", "Disk"]);
        disks.detach(&at);
        if edited {
            let edit = Change::Modified {
                path: "Disk/report.docx".into(),
            };
            assert_eq!(found.changes, [edit], "{kind}");
        }
        let created =
            (found.changes.iter()).filter(|change| matches!(change, Change::Created { .. }));
        assert_eq!(removals(&found.changes), Vec::<&Change>::new(), "{kind}");
        assert_eq!(created.count(), 0, "{kind}: {:?}", found.changes);
        assert!(
            away(&snapshot).is_empty(),
            "{kind}: the same stick plugged back in stays held"
        );
        assert!(snapshot.entries.keys().eq(expected.keys()), "{kind}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_stick_with_a_partition_table_has_the_id_of_the_volume_in_its_partition() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    if !disks.exfat_has_ids() {
        return;
    }
    let volume = disks.exfat("Card");
    let stick = disks.partitioned(&volume);
    let (whole, partition) = (disks.base().join("Whole"), disks.base().join("Partition"));
    fs::create_dir_all(&whole).unwrap();
    fs::create_dir_all(&partition).unwrap();
    disks.attach(&volume, &whole);
    let unpartitioned = identified(&whole);
    disks.detach(&whole);
    disks.attach(&stick, &partition);
    let partitioned = identified(&partition);
    disks.detach(&partition);
    assert!(partitioned.volume.is_some(), "{partitioned:?}");
    assert_eq!(partitioned.volume, unpartitioned.volume);
}

#[cfg(target_os = "linux")]
fn swapped_for_a_disk_with_one_uuid(
    formatted_at: [u64; 2],
    filled: bool,
) -> (Snapshot, Vec<Change>) {
    let mut disks = Disks::new().unwrap();
    let uuid = "4b6f6964-7269-4576-8500-000000000905";
    let [one, two] = [("One", formatted_at[0]), ("Two", formatted_at[1])]
        .map(|(name, at)| disks.ext4_with_uuid(name, uuid, at));
    let root = disks.base().join("Sync");
    let at = root.join("Disk");
    fs::create_dir_all(&at).unwrap();
    disks.attach(&one, &at);
    if filled {
        fill(&at);
    }
    let mut snapshot = Snapshot::scan(&root).unwrap();
    assert!(mounted(&snapshot, "Disk"));
    disks.detach(&at);
    rescan(&mut snapshot, &root, &["", "Disk"]);
    disks.attach(&two, &at);
    write(&at.join("photo.jpg"), "photo");
    let found = rescan(&mut snapshot, &root, &["", "Disk"]);
    disks.detach(&at);
    (snapshot, found.changes)
}

#[cfg(target_os = "linux")]
#[test]
fn an_empty_disk_formatted_with_another_disks_uuid_at_another_time_is_told_apart_by_its_root() {
    if Disks::new().is_none() {
        return;
    }
    let (snapshot, changes) =
        swapped_for_a_disk_with_one_uuid([1_700_000_000, 1_700_000_100], false);
    assert_eq!(changes, []);
    assert_eq!(away(&snapshot), ["Disk"]);
    assert!(!snapshot.entries.contains_key(Path::new("Disk/photo.jpg")));
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
    assert!(mounted(&snapshot, "B"));

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
fn an_unreadable_folder_moved_on_one_copy_of_a_disk_keeps_what_it_held_there() {
    if !permissions_block_reads() {
        return;
    }
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let (one, two) = clones(&mut disks);
    for (image, at) in [(&one, "A"), (&two, "B")] {
        fs::create_dir_all(root.join(at)).unwrap();
        disks.attach(image, &root.join(at));
    }
    write(&root.join("A/S/only-a"), "a");
    write(&root.join("B/S/only-b"), "b");
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(root.join("B/S"), root.join("B/T")).unwrap();
    fs::set_permissions(root.join("B/T"), fs::Permissions::from_mode(0o0)).unwrap();
    let found = rescan(&mut snapshot, &root, &[""]);
    fs::set_permissions(root.join("B/T"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(found.unreadable, [PathBuf::from("B/T")]);
    assert_eq!(
        found.changes,
        [Change::Renamed {
            from: "B/S".into(),
            to: "B/T".into(),
            kind: EntryKind::Dir
        }]
    );
    holds(&snapshot, &["B/T/only-b", "B/T/f.txt", "A/S/f.txt"]);
    assert!(!snapshot.entries.contains_key(Path::new("B/T/only-a")));
}

#[cfg(target_os = "linux")]
#[test]
fn a_disk_ejected_from_a_folder_moved_on_one_copy_of_another_disk_stays_there() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let (one, two) = clones(&mut disks);
    let inner = disks.image("Inner");
    for (image, at) in [(&one, "A"), (&two, "B")] {
        fs::create_dir_all(root.join(at)).unwrap();
        disks.attach(image, &root.join(at));
    }
    fs::create_dir(root.join("B/S/Disk")).unwrap();
    disks.attach(&inner, &root.join("B/S/Disk"));
    fill(&root.join("B/S/Disk"));
    let mut snapshot = Snapshot::scan(&root).unwrap();
    disks.detach(&root.join("B/S/Disk"));
    fs::rename(root.join("B/S"), root.join("B/T")).unwrap();
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(
        found.unreadable,
        [PathBuf::from("B/T/Disk")],
        "{:?}",
        found.changes
    );
    holds(
        &snapshot,
        &below("B/T/Disk")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    assert!(!snapshot.entries.contains_key(Path::new("A/S/Disk")));
    assert_eq!(away(&snapshot), ["B/T/Disk"]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_disk_ejected_from_another_disk_stays_there_when_a_folder_above_both_moves() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let outer = disks.image("Outer");
    let inner = disks.image("Inner");
    fs::create_dir_all(root.join("P/A")).unwrap();
    disks.attach(&outer, &root.join("P/A"));
    fs::create_dir(root.join("P/A/B")).unwrap();
    disks.attach(&inner, &root.join("P/A/B"));
    fill(&root.join("P/A/B"));
    let mut snapshot = Snapshot::scan(&root).unwrap();
    disks.detach(&root.join("P/A/B"));
    fs::rename(root.join("P"), root.join("Q")).unwrap();
    disks.moved(&root.join("P"), &root.join("Q"));
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(
        found.changes,
        [Change::Renamed {
            from: "P".into(),
            to: "Q".into(),
            kind: EntryKind::Dir
        }]
    );
    holds(
        &snapshot,
        &below("Q/A/B")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    assert_eq!(away(&snapshot), ["Q/A/B"]);
}

#[test]
fn an_unreadable_folder_on_a_disk_in_a_moved_folder_keeps_what_it_held() {
    if !permissions_block_reads() {
        return;
    }
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let disk = disks.image("Outer");
    fs::create_dir_all(root.join("P/A")).unwrap();
    disks.attach(&disk, &root.join("P/A"));
    write(&root.join("P/A/S/f.txt"), "f");
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::rename(root.join("P"), root.join("Q")).unwrap();
    disks.moved(&root.join("P"), &root.join("Q"));
    fs::set_permissions(root.join("Q/A/S"), fs::Permissions::from_mode(0o0)).unwrap();
    let found = rescan(&mut snapshot, &root, &[""]);
    fs::set_permissions(root.join("Q/A/S"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(root.join("Q"), root.join("P")).unwrap();
    disks.moved(&root.join("Q"), &root.join("P"));
    assert_eq!(found.unreadable, [PathBuf::from("Q/A/S")]);
    assert_eq!(
        found.changes,
        [Change::Renamed {
            from: "P".into(),
            to: "Q".into(),
            kind: EntryKind::Dir
        }]
    );
    holds(&snapshot, &["Q/A/S/f.txt"]);
}

#[test]
fn a_disk_whose_root_cannot_be_opened_is_read_once_it_can_be() {
    if !permissions_block_reads() {
        return;
    }
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let disk = disks.image("Locked");
    fs::create_dir_all(root.join("Disk")).unwrap();
    disks.attach(&disk, &root.join("Disk"));
    fill(&root.join("Disk"));
    fs::set_permissions(root.join("Disk"), fs::Permissions::from_mode(0o0)).unwrap();
    let scanned = Snapshot::scan(&root);
    fs::set_permissions(root.join("Disk"), fs::Permissions::from_mode(0o755)).unwrap();
    let mut snapshot = scanned.unwrap();
    rescan(&mut snapshot, &root, &[""]);
    assert!(mounted(&snapshot, "Disk"));
    holds(
        &snapshot,
        &below("Disk").iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(away(&snapshot), Vec::<&str>::new());
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
        assert!(mounted(&snapshot, &format!("{to}/q/m")));
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

fn moved_above_a_volume_mounted_over_synced_files(scanned_before_the_move: bool) {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let disk = disks.image("Over");
    write(&root.join("p/Data/keep.docx"), "keep");
    let mut snapshot = Snapshot::scan(&root).unwrap();
    disks.attach(&disk, &root.join("p/Data"));
    write(&root.join("p/Data/vol.txt"), "on the disk");
    if scanned_before_the_move {
        assert_eq!(rescan(&mut snapshot, &root, &[""]).changes, []);
    }
    fs::rename(root.join("p"), root.join("p2")).unwrap();
    disks.moved(&root.join("p"), &root.join("p2"));
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(
        found.changes,
        [Change::Renamed {
            from: "p".into(),
            to: "p2".into(),
            kind: EntryKind::Dir
        }]
    );
    holds(&snapshot, &["p2/Data/keep.docx"]);
    assert!(!snapshot.entries.contains_key(Path::new("p2/Data/vol.txt")));
    assert_eq!(
        snapshot.detached.get(Path::new("p2/Data")),
        Some(&Detached::Covered)
    );

    fs::rename(root.join("p2"), root.join("p")).unwrap();
    disks.moved(&root.join("p2"), &root.join("p"));
    disks.detach(&root.join("p/Data"));
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(
        found.changes,
        [Change::Renamed {
            from: "p2".into(),
            to: "p".into(),
            kind: EntryKind::Dir
        }]
    );
    assert!(snapshot.detached.is_empty());
    holds(&snapshot, &["p/Data/keep.docx"]);
}

#[test]
fn a_folder_above_a_volume_mounted_over_synced_files_moves_with_them() {
    moved_above_a_volume_mounted_over_synced_files(true);
}

#[test]
fn a_folder_moved_while_a_volume_is_mounted_over_synced_files_in_it_keeps_them() {
    moved_above_a_volume_mounted_over_synced_files(false);
}

#[test]
fn a_volume_known_as_a_plain_folder_is_taken_as_that_volume() {
    let Some(mut disks) = Disks::new() else {
        return;
    };
    let root = disks.base().join("Sync");
    let disk = disks.image("Known");
    fs::create_dir_all(root.join("Disk")).unwrap();
    disks.attach(&disk, &root.join("Disk"));
    fill(&root.join("Disk"));
    let mut snapshot = Snapshot::scan(&root).unwrap();
    snapshot.entries.get_mut(Path::new("Disk")).unwrap().mount = None;
    assert_eq!(rescan(&mut snapshot, &root, &[""]).changes, []);
    assert!(mounted(&snapshot, "Disk"));
    assert!(snapshot.detached.is_empty());

    disks.detach(&root.join("Disk"));
    let found = rescan(&mut snapshot, &root, &[""]);
    assert_eq!(removals(&found.changes), Vec::<&Change>::new());
    assert_eq!(away(&snapshot), ["Disk"]);
    holds(
        &snapshot,
        &below("Disk").iter().map(String::as_str).collect::<Vec<_>>(),
    );
}

fn scans_of_paths_inside(first: &Path, second: &Path, scanned_while_away: bool) {
    let mut disks = Disks::new().unwrap();
    let root = disks.base().join("Sync");
    fs::create_dir_all(root.join("Disk")).unwrap();
    disks.attach(first, &root.join("Disk"));
    fill(&root.join("Disk"));
    let mut snapshot = Snapshot::scan(&root).unwrap();
    disks.detach(&root.join("Disk"));
    if scanned_while_away {
        rescan(&mut snapshot, &root, &[""]);
    }
    disks.attach(second, &root.join("Disk"));
    write(&root.join("Disk/other.txt"), "other");
    for paths in [
        &["Disk/report.docx", "Disk/other.txt"][..],
        &["Disk/Slides/deck.key"],
        &[""],
    ] {
        let found = rescan(&mut snapshot, &root, paths);
        assert_eq!(
            removals(&found.changes),
            Vec::<&Change>::new(),
            "{first:?} {paths:?}"
        );
        assert!(
            !snapshot.entries.contains_key(Path::new("Disk/other.txt")),
            "{first:?} {paths:?}"
        );
        holds(
            &snapshot,
            &below("Disk").iter().map(String::as_str).collect::<Vec<_>>(),
        );
    }
    assert_eq!(away(&snapshot), ["Disk"]);
    disks.detach(&root.join("Disk"));
}

#[test]
fn a_scan_of_paths_inside_a_mount_point_reads_nothing_from_another_disk_there() {
    let Some(disks) = Disks::new() else {
        return;
    };
    let pairs = vec![
        (disks.image("First"), disks.image("Second")),
        (disks.fat("FATONE"), disks.fat("FATTWO")),
        #[cfg(target_os = "macos")]
        (disks.exfat("EXONE"), disks.exfat("EXTWO")),
    ];
    for (first, second) in &pairs {
        for scanned_while_away in [true, false] {
            scans_of_paths_inside(first, second, scanned_while_away);
        }
    }
}
