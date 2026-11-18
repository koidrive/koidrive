// SPDX-License-Identifier: Apache-2.0

/// The user moves `source/file` into the ignored folder `ignored` and removes `source`, and one batch reports both,
/// in either order. The file is outside what this device syncs, so it stays on the server, and so does `source`.
#[test]
fn a_file_moved_into_an_ignored_folder_stays_on_the_server_when_its_folder_is_removed_in_the_same_batch()
 {
    for removed_first in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::create_dir(site.root.join("source")).unwrap();
        a.create(&mut remote, &site.root.join("source/file"), b"kept");
        a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
        let id = a.observer.snapshot().entries[Path::new("source/file")].id;
        fs::create_dir(site.root.join("ignored")).unwrap();
        fs::rename(
            site.root.join("source/file"),
            site.root.join("ignored/file"),
        )
        .unwrap();
        fs::remove_dir(site.root.join("source")).unwrap();
        let renamed = Observed::Renamed {
            from: "source/file".into(),
            to: "ignored/file".into(),
            kind: EntryKind::File,
            id,
            linked: false,
        };
        let removed = Observed::Removed {
            path: "source".into(),
            kind: EntryKind::Dir,
        };
        let changes = match removed_first {
            false => vec![renamed, removed],
            true => vec![removed, renamed],
        };
        let sections = a.folder.sections();
        let batch = Batch {
            stamp: sections.stamp(),
            unread: (0, 0),
            answered: None,
            observation: crate::observe::Observation {
                changes,
                root: Ok(()),
                unreadable: Vec::new(),
                read: vec![PathBuf::new()],
                scanned: a.observer.scanned(),
                busy: Vec::new(),
                pending: Vec::new(),
                waiting: Default::default(),
                rules: Vec::new(),
                all_rules: false,
                mounts: BTreeMap::new(),
                detached: BTreeMap::new(),
                ctimes: BTreeMap::new(),
            },
        };
        let step = a.folder.observed(batch, 1);
        a.take(step).unwrap();
        a.run(&mut remote);
        assert_eq!(fs::read(site.root.join("ignored/file")).unwrap(), b"kept");
        assert_eq!(
            remote.names(),
            [".koiignore", "file", "source"],
            "{removed_first}"
        );
        assert_eq!(a.folder.engine.trees.check(), Ok(()));
    }
}

/// As above, with the move and the removal in two batches, and a restart after them: the restart scans the folder
/// again from L, which still holds `source/file`, and finds the file in the ignored folder again.
#[test]
fn a_file_moved_into_an_ignored_folder_stays_on_the_server_when_its_folder_is_removed_later_and_after_a_restart()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/file"), b"kept");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::rename(
        site.root.join("source/file"),
        site.root.join("ignored/file"),
    )
    .unwrap();
    a.scan_until(|a| !a.folder.engine.trees.ignored_local().is_empty());
    a.run(&mut remote);
    fs::remove_dir(site.root.join("source")).unwrap();
    a.scan_until(|a| (a.folder.engine.trees.ignored_local().len()) == 2);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "file", "source"]);
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
    drop(a);

    let mut a = site.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(fs::read(site.root.join("ignored/file")).unwrap(), b"kept");
    assert_eq!(remote.names(), [".koiignore", "file", "source"]);
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// The user moves `source/file` into the ignored folder and removes `source`, so L keeps `source` for the file.
/// Then the user creates a folder named `source` again and a file in it. The new folder is the one L keeps, so the
/// server keeps `file` in it and adds the new file there, also after a restart.
#[test]
fn a_folder_created_again_at_a_removed_folder_kept_for_an_ignored_file_keeps_the_file_on_the_server()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/file"), b"kept");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::rename(
        site.root.join("source/file"),
        site.root.join("ignored/file"),
    )
    .unwrap();
    fs::remove_dir(site.root.join("source")).unwrap();
    a.scan_until(|a| (a.folder.engine.trees.ignored_local().len()) == 2);
    a.run(&mut remote);
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/new"), b"new");
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "file", "new", "source"]);
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
    drop(a);

    let mut a = site.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(fs::read(site.root.join("ignored/file")).unwrap(), b"kept");
    assert_eq!(fs::read(site.root.join("source/new")).unwrap(), b"new");
    assert_eq!(remote.names(), [".koiignore", "file", "new", "source"]);
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// As above, but another device deletes `source` with `file` in it, and this device takes in the deletion before the
/// user creates `source` again. The new folder is a new node: the deleted folder and file stay in the trash, with or
/// without a restart in between.
#[test]
fn a_folder_created_again_at_a_kept_folder_another_device_deleted_is_a_new_folder() {
    for restart in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::create_dir(site.root.join("source")).unwrap();
        a.create(&mut remote, &site.root.join("source/file"), b"kept");
        a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
        let before = server_nodes(&remote);
        let (source, file) = (live_id(&before, "source"), live_id(&before, "file"));
        fs::create_dir(site.root.join("ignored")).unwrap();
        fs::rename(
            site.root.join("source/file"),
            site.root.join("ignored/file"),
        )
        .unwrap();
        fs::remove_dir(site.root.join("source")).unwrap();
        a.scan_until(|a| (a.folder.engine.trees.ignored_local().len()) == 2);
        a.run(&mut remote);
        let (other, mut b) = linked(&mut remote, "b");
        b.settle();
        b.run(&mut remote);
        b.settle();
        b.run(&mut remote);
        assert_eq!(fs::read(other.root.join("source/file")).unwrap(), b"kept");
        fs::remove_dir_all(other.root.join("source")).unwrap();
        b.scan_until(|b| !b.work.is_empty());
        b.run(&mut remote);
        a.pull(&remote);
        a.run(&mut remote);
        a.settle();
        a.run(&mut remote);
        assert!(a.folder.engine.trees.remote().live(&source).is_none());
        if restart {
            drop(a);
            a = site.start();
            a.connect(&remote);
            a.scan_until(|a| a.folder.scanned);
            a.run(&mut remote);
        }
        fs::create_dir(site.root.join("source")).unwrap();
        fs::write(site.root.join("source/new"), b"new").unwrap();
        a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("source/new")));
        a.run(&mut remote);
        a.settle();
        a.run(&mut remote);
        let after = server_nodes(&remote);
        let revived = |id: &String| after.iter().any(|(_, node, live)| node == id && *live);
        assert!(!revived(&source) && !revived(&file), "{restart}: {after:?}");
        assert_eq!(remote.names(), [".koiignore", "new", "source"], "{restart}");
        assert_eq!(fs::read(site.root.join("ignored/file")).unwrap(), b"kept");
        assert_eq!(a.folder.engine.trees.check(), Ok(()));
    }
}

#[test]
fn a_file_created_at_a_removed_folder_kept_for_an_ignored_file_keeps_the_file_on_the_server() {
    a_name_l_keeps_is_taken(true, Taker::NewFile);
}

#[test]
fn a_folder_moved_onto_a_removed_folder_kept_for_an_ignored_file_keeps_the_file_on_the_server() {
    a_name_l_keeps_is_taken(true, Taker::MovedFolder);
}

#[test]
fn a_synced_file_moved_onto_a_removed_folder_kept_for_an_ignored_file_keeps_the_file_on_the_server()
{
    a_name_l_keeps_is_taken(true, Taker::MovedFile);
}

#[test]
fn a_file_created_at_the_name_of_a_file_moved_into_an_ignored_folder_keeps_the_file_on_the_server()
{
    a_name_l_keeps_is_taken(false, Taker::NewFile);
}

/// The user moves `source/file` into the ignored folder, removes `source`, creates `source` again and writes a new
/// `source/file`. The new file is not the one in the ignored folder, so the server keeps that file's content under
/// its node and takes the new file as another node.
#[test]
fn a_file_created_in_a_folder_created_again_does_not_replace_the_file_moved_into_an_ignored_folder()
{
    for restart in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::create_dir(site.root.join("source")).unwrap();
        a.create(&mut remote, &site.root.join("source/file"), b"kept");
        a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
        let file = live_id(&server_nodes(&remote), "file");
        fs::create_dir(site.root.join("ignored")).unwrap();
        fs::rename(
            site.root.join("source/file"),
            site.root.join("ignored/file"),
        )
        .unwrap();
        fs::remove_dir(site.root.join("source")).unwrap();
        a.scan_until(|a| a.folder.engine.trees.ignored_local().len() == 2);
        a.run(&mut remote);
        if restart {
            drop(a);
            a = site.start();
            a.connect(&remote);
            a.scan_until(|a| a.folder.scanned);
            a.run(&mut remote);
        }
        fs::create_dir(site.root.join("source")).unwrap();
        fs::write(site.root.join("source/file"), b"taker").unwrap();
        for _ in 0..3 {
            a.settle();
            a.run(&mut remote);
        }
        let after = server_nodes(&remote);
        assert_eq!(live_id(&after, "file"), file, "{restart}: {after:?}");
        assert_eq!(server_size(&remote, &file), 4, "{restart}");
        let taker = live_id(&after, "file (conflict from a 2023-11-14)");
        assert_eq!(server_size(&remote, &taker), 5, "{restart}");
        assert_eq!(fs::read(site.root.join("ignored/file")).unwrap(), b"kept");
        assert_eq!(a.folder.engine.trees.check(), Ok(()));
    }
}

/// Another entry took the name of `source/file` while the file was in the ignored folder. When the user moves the
/// file back out, to a path that is synced, it is the same node again: the server moves it and takes no second copy,
/// also when the device restarted in between.
#[test]
fn a_file_moved_back_out_of_an_ignored_folder_after_its_name_was_taken_is_moved_on_the_server() {
    for restart in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::create_dir(site.root.join("source")).unwrap();
        a.create(&mut remote, &site.root.join("source/file"), b"kept");
        a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
        let file = live_id(&server_nodes(&remote), "file");
        fs::create_dir(site.root.join("ignored")).unwrap();
        fs::rename(
            site.root.join("source/file"),
            site.root.join("ignored/file"),
        )
        .unwrap();
        a.scan_until(|a| a.folder.engine.trees.ignored_local().len() == 1);
        a.run(&mut remote);
        fs::create_dir(site.root.join("source/file")).unwrap();
        a.scan_until(|a| a.folder.engine.trees.displaced_local().len() == 1);
        for _ in 0..2 {
            a.run(&mut remote);
            a.settle();
        }
        if restart {
            drop(a);
            a = site.start();
            a.connect(&remote);
            a.scan_until(|a| a.folder.scanned);
            a.run(&mut remote);
        }
        fs::rename(site.root.join("ignored/file"), site.root.join("back")).unwrap();
        for _ in 0..3 {
            a.settle();
            a.run(&mut remote);
        }
        let after = server_nodes(&remote);
        assert_eq!(live_id(&after, "back"), file, "{restart}: {after:?}");
        assert!(a.folder.engine.trees.displaced_local().is_empty());
        assert_eq!(fs::read(site.root.join("back")).unwrap(), b"kept");
        assert_eq!(a.folder.engine.trees.check(), Ok(()));
    }
}

#[test]
fn a_link_of_a_file_moved_into_an_ignored_folder_after_its_name_was_taken_is_moved_on_the_server() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/file"), b"kept");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    let file = live_id(&server_nodes(&remote), "file");
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::rename(
        site.root.join("source/file"),
        site.root.join("ignored/file"),
    )
    .unwrap();
    a.scan_until(|a| a.folder.engine.trees.ignored_local().len() == 1);
    a.run(&mut remote);
    fs::create_dir(site.root.join("source/file")).unwrap();
    a.scan_until(|a| a.folder.engine.trees.displaced_local().len() == 1);
    for _ in 0..2 {
        a.run(&mut remote);
        a.settle();
    }
    fs::hard_link(site.root.join("ignored/file"), site.root.join("back")).unwrap();
    for _ in 0..3 {
        a.settle();
        a.run(&mut remote);
    }
    let after = server_nodes(&remote);
    assert_eq!(live_id(&after, "back"), file, "{after:?}");
    assert!(a.folder.engine.trees.displaced_local().is_empty());
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

#[test]
fn a_folder_created_at_the_name_of_a_file_moved_into_an_ignored_folder_keeps_the_file_on_the_server()
 {
    a_name_l_keeps_is_taken(false, Taker::NewFolder);
}

#[test]
fn a_folder_moved_onto_the_name_of_a_file_moved_into_an_ignored_folder_keeps_the_file_on_the_server()
 {
    a_name_l_keeps_is_taken(false, Taker::MovedFolder);
}

#[test]
fn a_synced_file_moved_onto_the_name_of_a_file_moved_into_an_ignored_folder_keeps_the_file_on_the_server()
 {
    a_name_l_keeps_is_taken(false, Taker::MovedFile);
}

/// The user hard-links `source/file` into the ignored folder, moves the file itself to `moved`, and removes
/// `source`. The file is still synced at `moved`, so the server follows the move; the link stays out.
#[test]
fn a_file_moved_out_of_a_removed_folder_with_a_link_in_an_ignored_folder_is_moved_on_the_server() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/file"), b"kept");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::hard_link(
        site.root.join("source/file"),
        site.root.join("ignored/alias"),
    )
    .unwrap();
    fs::rename(site.root.join("source/file"), site.root.join("moved")).unwrap();
    fs::remove_dir(site.root.join("source")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("moved")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "moved"]);
    assert_eq!(fs::read(site.root.join("moved")).unwrap(), b"kept");
    assert_eq!(fs::read(site.root.join("ignored/alias")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

#[test]
fn a_file_with_a_link_in_an_ignored_folder_moved_back_in_after_its_folder_was_removed_is_moved_on_the_server()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/file"), b"kept");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::hard_link(
        site.root.join("source/file"),
        site.root.join("ignored/alias"),
    )
    .unwrap();
    let away = site.root.with_file_name("away");
    fs::rename(site.root.join("source/file"), &away).unwrap();
    fs::remove_dir(site.root.join("source")).unwrap();
    a.scan_until(|a| !(a.observer.snapshot().entries).contains_key(Path::new("source")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert!(
        remote.names().contains(&"file".to_owned()),
        "{:?}",
        remote.names()
    );
    fs::rename(&away, site.root.join("moved")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("moved")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "moved"]);
    assert_eq!(fs::read(site.root.join("moved")).unwrap(), b"kept");
    assert_eq!(fs::read(site.root.join("ignored/alias")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

#[test]
fn two_links_moved_into_an_ignored_folder_and_linked_back_in_turn_keep_their_nodes() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ig/\n");
    a.create(&mut remote, &site.root.join("one"), b"kept");
    fs::hard_link(site.root.join("one"), site.root.join("two")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("two")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    let before = server_nodes(&remote);
    let (one, two) = (live_id(&before, "one"), live_id(&before, "two"));
    fs::create_dir(site.root.join("ig")).unwrap();
    fs::rename(site.root.join("one"), site.root.join("ig/one")).unwrap();
    fs::rename(site.root.join("two"), site.root.join("ig/two")).unwrap();
    a.scan_until(|a| !(a.observer.snapshot().entries).contains_key(Path::new("two")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    for back in ["back1", "back2"] {
        fs::hard_link(site.root.join("ig/one"), site.root.join(back)).unwrap();
        a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new(back)));
        a.run(&mut remote);
        a.settle();
        a.run(&mut remote);
    }
    let after = server_nodes(&remote);
    assert_eq!(remote.names(), [".koiignore", "back1", "back2"]);
    assert_eq!(
        BTreeSet::from([live_id(&after, "back1"), live_id(&after, "back2")]),
        BTreeSet::from([one, two])
    );
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

#[test]
fn a_file_moved_back_in_from_an_ignored_folder_after_its_folder_was_removed_takes_the_folder_off_the_server()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/file"), b"kept");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::rename(
        site.root.join("source/file"),
        site.root.join("ignored/file"),
    )
    .unwrap();
    fs::remove_dir(site.root.join("source")).unwrap();
    a.scan_until(|a| !(a.observer.snapshot().entries).contains_key(Path::new("source")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "file", "source"]);
    fs::rename(site.root.join("ignored/file"), site.root.join("moved")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("moved")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "moved"]);
    assert_eq!(fs::read(site.root.join("moved")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

#[test]
fn a_folder_removed_while_its_file_moves_out_and_its_ignored_file_comes_back_leaves_the_server() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/file"), b"kept");
    a.create(&mut remote, &site.root.join("source/other"), b"other");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::rename(
        site.root.join("source/file"),
        site.root.join("ignored/file"),
    )
    .unwrap();
    a.scan_until(|a| a.folder.engine.trees.ignored_local().len() == 1);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    fs::create_dir(site.root.join("new")).unwrap();
    fs::rename(site.root.join("source/other"), site.root.join("new/other")).unwrap();
    fs::rename(site.root.join("ignored/file"), site.root.join("moved")).unwrap();
    fs::remove_dir(site.root.join("source")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("new/other")));
    for _ in 0..2 {
        a.run(&mut remote);
        a.settle();
    }
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "moved", "new", "other"]);
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

#[test]
fn a_folder_moved_into_an_ignored_folder_after_its_file_stays_on_the_server_when_the_file_comes_back()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("D")).unwrap();
    a.create(&mut remote, &site.root.join("D/c"), b"kept");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::rename(site.root.join("D/c"), site.root.join("ignored/c")).unwrap();
    a.scan_until(|a| !(a.observer.snapshot().entries).contains_key(Path::new("D/c")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    fs::rename(site.root.join("D"), site.root.join("ignored/D")).unwrap();
    a.scan_until(|a| !(a.observer.snapshot().entries).contains_key(Path::new("D")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    fs::rename(site.root.join("ignored/c"), site.root.join("moved")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("moved")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert!(site.root.join("ignored/D").is_dir());
    assert_eq!(remote.names(), [".koiignore", "D", "moved"]);
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// The file moved into an ignored folder has a synced link, `also`, that stays. The file is still left out, so
/// the server keeps it and its folder.
#[test]
fn a_file_moved_into_an_ignored_folder_out_of_a_removed_folder_with_an_included_link_that_stays_is_kept_on_the_server()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/file"), b"kept");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    fs::hard_link(site.root.join("source/file"), site.root.join("also")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "also", "file", "source"]);
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::rename(
        site.root.join("source/file"),
        site.root.join("ignored/file"),
    )
    .unwrap();
    fs::remove_dir(site.root.join("source")).unwrap();
    a.scan_until(|a| !(a.observer.snapshot().entries).contains_key(Path::new("source")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "also", "file", "source"]);
    assert_eq!(fs::read(site.root.join("ignored/file")).unwrap(), b"kept");
    assert_eq!(fs::read(site.root.join("also")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// The file is moved into an ignored folder, its own folder is removed, and the folder above that is moved to
/// `dest`. The move of that folder does not carry the file, so the server keeps it.
#[test]
fn a_file_moved_into_an_ignored_folder_before_a_folder_above_it_was_moved_is_kept_on_the_server() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site.root.join("d/a/b")).unwrap();
    fs::create_dir(site.root.join("dest")).unwrap();
    a.create(&mut remote, &site.root.join("d/a/b/file"), b"kept");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ig/\n");
    fs::create_dir(site.root.join("ig")).unwrap();
    fs::rename(site.root.join("d/a/b/file"), site.root.join("ig/file")).unwrap();
    fs::remove_dir(site.root.join("d/a/b")).unwrap();
    fs::rename(site.root.join("d/a"), site.root.join("dest/a")).unwrap();
    fs::remove_dir(site.root.join("d")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("dest/a")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert!(
        remote.names().contains(&"file".to_owned()),
        "{:?}",
        remote.names()
    );
    assert_eq!(fs::read(site.root.join("ig/file")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// The user moves the folder `d/a` into the ignored folder `ig`, moves its file from there into the ignored folder
/// `other`, brings the empty folder back as `returned` and removes it. The file stays out the whole time, so the
/// server keeps it, with the folder it is in there.
#[test]
fn a_file_moved_out_of_its_folder_while_the_folder_was_ignored_stays_on_the_server_when_the_folder_returns_and_is_removed()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site.root.join("d/a")).unwrap();
    a.create(&mut remote, &site.root.join("d/a/file"), b"kept");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ig/\nother/\n");
    fs::create_dir(site.root.join("ig")).unwrap();
    fs::create_dir(site.root.join("other")).unwrap();
    fs::rename(site.root.join("d/a"), site.root.join("ig/a")).unwrap();
    fs::rename(site.root.join("ig/a/file"), site.root.join("other/file")).unwrap();
    a.scan_until(|a| !a.folder.engine.trees.ignored_local().is_empty());
    a.run(&mut remote);
    fs::rename(site.root.join("ig/a"), site.root.join("returned")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("returned")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "d", "file", "returned"]);
    fs::remove_dir(site.root.join("returned")).unwrap();
    a.scan_until(|a| !(a.observer.snapshot().entries).contains_key(Path::new("returned")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "d", "file", "returned"]);
    assert_eq!(fs::read(site.root.join("other/file")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// As above, with a restart after the folder returns: the snapshot rebuilt from L holds the file in `returned`, and
/// the first scan finds it in `other`.
#[test]
fn a_file_moved_out_of_its_folder_while_the_folder_was_ignored_stays_on_the_server_after_a_restart()
{
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site.root.join("d/a")).unwrap();
    a.create(&mut remote, &site.root.join("d/a/file"), b"kept");
    a.create(&mut remote, &site.root.join(".koiignore"), b"ig/\nother/\n");
    fs::create_dir(site.root.join("ig")).unwrap();
    fs::create_dir(site.root.join("other")).unwrap();
    fs::rename(site.root.join("d/a"), site.root.join("ig/a")).unwrap();
    fs::rename(site.root.join("ig/a/file"), site.root.join("other/file")).unwrap();
    a.scan_until(|a| !a.folder.engine.trees.ignored_local().is_empty());
    a.run(&mut remote);
    fs::rename(site.root.join("ig/a"), site.root.join("returned")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("returned")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    drop(a);

    let mut a = site.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "d", "file", "returned"]);
    fs::remove_dir(site.root.join("returned")).unwrap();
    a.scan_until(|a| !(a.observer.snapshot().entries).contains_key(Path::new("returned")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "d", "file", "returned"]);
    assert_eq!(fs::read(site.root.join("other/file")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// Device `b` swaps two files through a third name. Each file keeps its node, and so its version history, and both
/// names arrive as they are, with no conflict name.
#[test]
fn two_files_swapped_through_a_third_name_keep_their_nodes() {
    let (site_a, site, before, after) = renamed_on_b(
        &[("one", b"first file"), ("two", b"second file")],
        &[("one", "tmp"), ("two", "one"), ("tmp", "two")],
    );
    let swapped = [("one", "second file"), ("two", "first file")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), swapped);
    assert_eq!(files(&site.root), swapped);
    assert_eq!(after.len(), 2, "{after:?}");
    assert_eq!(live_id(&after, "one"), live_id(&before, "two"));
    assert_eq!(live_id(&after, "two"), live_id(&before, "one"));
}

/// As above for three files rotated through a third name.
#[test]
fn three_files_rotated_through_a_third_name_keep_their_nodes() {
    let (_, site, before, after) = renamed_on_b(
        &[("a", b"from a"), ("b", b"from b"), ("c", b"from c")],
        &[("a", "tmp"), ("c", "a"), ("b", "c"), ("tmp", "b")],
    );
    let rotated = [("a", "from c"), ("b", "from a"), ("c", "from b")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site.root), rotated);
    assert_eq!(after.len(), 3, "{after:?}");
    assert_eq!(live_id(&after, "a"), live_id(&before, "c"));
    assert_eq!(live_id(&after, "b"), live_id(&before, "a"));
    assert_eq!(live_id(&after, "c"), live_id(&before, "b"));
}

/// Device `b` swaps two folders that each hold a file named `note`. The folders and their files keep their nodes,
/// so nothing goes to the server's trash, and no name carries a conflict suffix.
#[test]
fn two_folders_swapped_through_a_third_name_keep_their_nodes() {
    let (site_a, site, before, after) = renamed_on_b(
        &[("x/note", b"in x"), ("y/note", b"in y")],
        &[("x", "tmp"), ("y", "x"), ("tmp", "y")],
    );
    let swapped = [("x/note", "in y"), ("y/note", "in x")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), swapped);
    assert_eq!(files(&site.root), swapped);
    assert!(after.iter().all(|(_, _, live)| *live), "{after:?}");
    let names: Vec<&str> = after.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(names, ["note", "note", "x", "y"]);
    assert_eq!(live_id(&after, "x"), live_id(&before, "y"));
    assert_eq!(live_id(&after, "y"), live_id(&before, "x"));
    let ids = |nodes: &[(String, String, bool)]| -> std::collections::BTreeSet<String> {
        nodes.iter().map(|(_, id, _)| id.clone()).collect()
    };
    assert_eq!(ids(&after), ids(&before));
}

/// An editor saves `doc` by writing a new file next to it, swapping the two, and removing the old one, and the
/// observer sees the new file before the swap. `doc` keeps its node, so its version history stays with it.
#[test]
fn a_file_saved_by_swapping_in_a_new_file_keeps_its_node() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("doc"), b"first");
    let before = server_nodes(&remote);
    a.event(Event::Disconnected {
        reason: koidrive_net::DisconnectReason::Timeout,
        retry_in: Duration::from_secs(1),
    })
    .unwrap();
    fs::write(site.root.join(".doc.tmp"), b"second").unwrap();
    a.scan_until(|a| a.folder.engine.trees.dirty().len() == 1);
    assert!(a.work.is_empty());
    for (from, to) in [(".doc.tmp", "x"), ("doc", ".doc.tmp"), ("x", "doc")] {
        fs::rename(site.root.join(from), site.root.join(to)).unwrap();
    }
    a.settle();
    fs::remove_file(site.root.join(".doc.tmp")).unwrap();
    a.settle();
    a.connect(&remote);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(files(&site.root), [("doc".to_owned(), "second".to_owned())]);
    let after = server_nodes(&remote);
    assert_eq!(live_id(&after, "doc"), live_id(&before, "doc"), "{after:?}");
}

#[test]
fn a_file_moved_into_an_ignored_folder_while_its_overwrite_is_held_stays_on_the_other_device() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let settled = |a: &mut Device, remote: &mut Remote| {
        for _ in 0..3 {
            a.settle();
            a.run(remote);
        }
    };
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    fs::create_dir(site.root.join("d")).unwrap();
    a.create(&mut remote, &site.root.join("d/f"), b"kept");
    a.create(&mut remote, &site.root.join("x"), b"other");
    fs::create_dir_all(site.root.join("ignored/zzz")).unwrap();
    for n in 0..240 {
        let dir = site.root.join(format!("ignored/d{n:03}"));
        fs::create_dir(&dir).unwrap();
        for m in 0..100 {
            fs::write(dir.join(format!("{m}")), b"x").unwrap();
        }
    }
    fs::hard_link(site.root.join("d/f"), site.root.join("ignored/zzz/l")).unwrap();
    settled(&mut a, &mut remote);
    let (site_b, mut b) = linked(&mut remote, "b");
    let caught_up = |b: &mut Device, remote: &mut Remote| {
        for _ in 0..30 {
            match b.work.pop_front() {
                Some(work) => b.perform(work, remote).unwrap(),
                None => b.pull(remote),
            }
            b.settle();
        }
    };
    caught_up(&mut b, &mut remote);

    fs::rename(site.root.join("d/f"), site.root.join("x")).unwrap();
    fs::remove_dir(site.root.join("d")).unwrap();
    a.scan(Duration::from_millis(300));
    a.run(&mut remote);
    fs::rename(site.root.join("x"), site.root.join("ignored/zzz/x")).unwrap();
    for _ in 0..3 {
        settled(&mut a, &mut remote);
    }
    caught_up(&mut b, &mut remote);
    let on_b = files(&site_b.root);
    assert!(
        on_b.iter().any(|(_, text)| text == "kept"),
        "{:?} {on_b:?}",
        server_nodes(&remote)
    );
}

#[test]
fn a_file_moved_below_an_ignored_folder_while_a_new_file_takes_its_name_stays_on_the_server_on_macos()
 {
    let conflict = "x (conflict from a 2023-11-14)";
    for (over, to) in [
        (false, "ignored/x"),
        (false, "ignored/zzz/x"),
        (true, "ignored/x"),
        (true, "ignored/zzz/x"),
    ] {
        let case = format!("{over} {to}");
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
        fs::create_dir(site.root.join("d")).unwrap();
        a.create(&mut remote, &site.root.join("d/f"), b"kept");
        a.create(&mut remote, &site.root.join("x"), b"other");
        fs::create_dir_all(site.root.join("ignored/zzz")).unwrap();
        a.rest(&mut remote);
        let content = if over { "kept" } else { "other" };
        if over {
            fs::rename(site.root.join("d/f"), site.root.join("x")).unwrap();
            fs::remove_dir(site.root.join("d")).unwrap();
            a.rest(&mut remote);
        }
        let synced = server_nodes(&remote);
        let moved = live_id(&synced, "x");
        assert_eq!(
            server_size(&remote, &moved),
            content.len() as u64,
            "{case}: {synced:?}"
        );
        assert_eq!(
            synced.iter().filter(|(_, _, live)| *live).count(),
            if over { 2 } else { 4 },
            "{case}: {synced:?}"
        );
        let (site_b, mut b) = linked(&mut remote, "b");
        b.pull(&remote);
        b.rest(&mut remote);
        fs::rename(site.root.join("x"), site.root.join(to)).unwrap();
        fs::write(site.root.join("x"), b"new").unwrap();
        a.rest(&mut remote);
        b.pull(&remote);
        b.rest(&mut remote);
        let after = server_nodes(&remote);
        let on_b: Vec<(String, String)> = files(&site_b.root)
            .into_iter()
            .filter(|(path, _)| path.starts_with('x'))
            .collect();
        assert_eq!(live_id(&after, "x"), moved, "{case}: {after:?}");
        if cfg!(target_os = "macos") {
            assert_eq!(server_size(&remote, &moved), content.len() as u64, "{case}");
            live_id(&after, conflict);
            assert_eq!(
                on_b,
                [
                    ("x".into(), content.into()),
                    (conflict.into(), "new".into())
                ],
                "{case}"
            );
            assert_eq!(
                fs::read(site.root.join(to)).unwrap(),
                content.as_bytes(),
                "{case}"
            );
        } else {
            assert_eq!(server_size(&remote, &moved), 3, "{case}");
            assert_eq!(on_b, [("x".into(), "new".into())], "{case}");
        }
        assert_eq!(a.folder.engine.trees.check(), Ok(()), "{case}");
    }
}
