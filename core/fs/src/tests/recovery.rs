// SPDX-License-Identifier: Apache-2.0

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
        u128::from(meta.ino())
    };
    let ids = |space: &Space| {
        let BroughtBack::Entries(ids) = space.brought_back().unwrap() else {
            panic!("the record is unreadable");
        };
        let mut ids: Vec<_> = (ids.into_iter()).map(|id| id.ino).collect();
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
fn a_record_of_brought_back_entries_written_with_device_numbers_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let space = Space::bind(open_root(&root).unwrap(), &root).unwrap();
    let private = root.join(PRIVATE_DIR);
    fs::create_dir_all(&private).unwrap();
    fs::write(
        private.join("brought-back.json"),
        "[[16777231,9,null],[16777231,12,1700000000000000000]]",
    )
    .unwrap();

    let BroughtBack::Entries(ids) = space.brought_back().unwrap() else {
        panic!("the record is unreadable");
    };
    assert_eq!(
        ids,
        HashSet::from([
            FileId {
                ino: 9,
                birth_ns: None
            },
            FileId {
                ino: 12,
                birth_ns: Some(1_700_000_000_000_000_000)
            },
        ])
    );
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
        ino: 2,
        birth_ns: None
    }));

    space.repair_brought_back().unwrap();
    let id = |path: PathBuf| {
        let meta = fs::symlink_metadata(path).unwrap();
        u128::from(meta.ino())
    };
    let expected: std::collections::BTreeSet<_> =
        [id(root.join(&moved[0])), id(root.join("docs"))].into();
    let BroughtBack::Entries(repaired) = space.brought_back().unwrap() else {
        panic!("the record is still unreadable");
    };
    let repaired: std::collections::BTreeSet<_> = repaired.into_iter().map(|id| id.ino).collect();
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
