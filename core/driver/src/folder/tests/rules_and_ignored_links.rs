// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_file_unreadable_until_its_folder_swapped_names_reaches_the_other_device() {
    use std::os::unix::fs::PermissionsExt;

    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("a")).unwrap();
    fs::create_dir(site_a.root.join("b")).unwrap();
    a.create(&mut remote, &site_a.root.join("a/one"), b"one");
    a.create(&mut remote, &site_a.root.join("b/two"), b"two");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    let locked = site.root.join("a/locked");
    fs::write(&locked, b"locked").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&locked).is_ok() {
        assert!(rustix::process::geteuid().is_root());
        eprintln!("SKIPPED: running as root, so permissions do not block reads");
        return;
    }
    b.rest(&mut remote);
    for (from, to) in [("a", "t"), ("b", "a"), ("t", "b")] {
        fs::rename(site.root.join(from), site.root.join(to)).unwrap();
    }
    fs::set_permissions(
        site.root.join("b/locked"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let moved = [("a/two", "two"), ("b/locked", "locked"), ("b/one", "one")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site.root), moved);
    assert_eq!(files(&site_a.root), moved);
}

#[test]
fn a_held_folder_another_devices_rules_leave_out_stays_held_when_renamed() {
    let mut remote = Remote::new();
    let site = synced_before(&mut remote, "b");
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");
    fs::create_dir(site.root.join("held")).unwrap();
    fs::write(site.root.join("held/a.txt"), b"inside").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.create(&mut remote, &site_a.root.join(".koiignore"), b"held/\n");
    b.pull(&remote);
    b.run(&mut remote);
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    b.run(&mut remote);
    fs::rename(site.root.join("held"), site.root.join("open")).unwrap();
    b.rest(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "notes.txt"]);
}

#[test]
fn a_folder_that_returns_from_a_path_the_rules_exclude_keeps_its_node() {
    let cases: [(&[&str], &str, &str); 5] = [
        (&["docs/x.txt", "docs/sub/y.txt"], "docs", "docs2"),
        (&["a/docs/sub/y.txt"], "a/docs", "a/notes"),
        (&["a/docs/x.txt", "b/z.txt"], "a/docs", "b/notes"),
        (&["docs/sub/y.txt"], "docs/sub", "sub"),
        (&["docs/sub/deep/y.txt", "b/z.txt"], "docs/sub", "b/sub"),
    ];
    let ways = [
        "while running",
        "with its rule lifted",
        "while stopped",
        "while stopped with its rule lifted",
    ];
    for ((made, from, to), way) in cases
        .into_iter()
        .flat_map(|case| ways.map(|way| (case, way)))
    {
        let mut remote = Remote::new();
        let (site, mut b) = linked(&mut remote, "b");
        for file in made {
            let path = site.root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file.as_bytes()).unwrap();
        }
        for rules in [&b""[..], b"docs/\n"] {
            fs::write(site.root.join(".koiignore"), rules).unwrap();
            b.rest(&mut remote);
        }
        let before = remote.tree();
        assert!(before.contains_key(from), "{from} is synced: {before:?}");
        if way.ends_with("with its rule lifted") {
            fs::write(site.root.join(".koiignore"), b"").unwrap();
        }
        if way.starts_with("while stopped") {
            drop(b);
            fs::rename(site.root.join(from), site.root.join(to)).unwrap();
            b = restarted(&site, &mut remote);
        } else {
            fs::rename(site.root.join(from), site.root.join(to)).unwrap();
        }
        b.rest(&mut remote);
        let moved = |path: &String| match path.strip_prefix(from) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{to}{rest}"),
            _ => path.clone(),
        };
        let expected: BTreeMap<String, String> = (before.iter())
            .map(|(path, id)| (moved(path), id.clone()))
            .collect();
        assert_eq!(remote.tree(), expected, "{from} -> {to} {way}");
    }
}

#[test]
fn a_held_folder_renamed_while_another_devices_rule_lets_it_in_stays_held() {
    let mut remote = Remote::new();
    let site = synced_before(&mut remote, "b");
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");
    fs::create_dir(site.root.join("held")).unwrap();
    fs::write(site.root.join("held/a.txt"), b"inside").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.create(&mut remote, &site_a.root.join(".koiignore"), b"held/\n");
    b.pull(&remote);
    b.run(&mut remote);
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    b.run(&mut remote);
    a.create(&mut remote, &site_a.root.join(".koiignore"), b"other.txt\n");
    fs::rename(site.root.join("held"), site.root.join("open")).unwrap();
    b.pull(&remote);
    b.run(&mut remote);
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    b.rest(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "notes.txt"]);
    assert_eq!(b.folder.view().questions, 1);
}

#[test]
fn a_held_folder_another_devices_rules_leave_out_stays_held_when_renamed_while_stopped() {
    let mut remote = Remote::new();
    let site = synced_before(&mut remote, "b");
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");
    fs::create_dir(site.root.join("held")).unwrap();
    fs::write(site.root.join("held/a.txt"), b"inside").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.create(&mut remote, &site_a.root.join(".koiignore"), b"held/\n");
    b.pull(&remote);
    b.run(&mut remote);
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    b.run(&mut remote);
    drop(b);
    fs::rename(site.root.join("held"), site.root.join("open")).unwrap();
    let mut b = restarted(&site, &mut remote);
    b.rest(&mut remote);
    assert_eq!(remote.names(), [".koiignore", "notes.txt"]);
}

#[test]
fn a_link_moved_into_an_ignored_folder_beside_an_included_link_keeps_its_node() {
    for (made, link) in [("source/file", "also"), ("also", "source/file")] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        let settled = |a: &mut Device, remote: &mut Remote| {
            for _ in 0..3 {
                a.settle();
                a.run(remote);
            }
        };
        a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
        fs::create_dir(site.root.join("source")).unwrap();
        a.create(&mut remote, &site.root.join(made), b"kept");
        fs::hard_link(site.root.join(made), site.root.join(link)).unwrap();
        settled(&mut a, &mut remote);
        let before = server_nodes(&remote);
        let (file, also) = (live_id(&before, "file"), live_id(&before, "also"));
        let live =
            |nodes: &Nodes, id: &str| nodes.iter().any(|(_, node, live)| node == id && *live);

        fs::create_dir(site.root.join("ignored")).unwrap();
        fs::rename(
            site.root.join("source/file"),
            site.root.join("ignored/file"),
        )
        .unwrap();
        settled(&mut a, &mut remote);
        let moved = server_nodes(&remote);
        assert!(
            live(&moved, &file) && live(&moved, &also),
            "{made}: {moved:?}"
        );
        assert_eq!(a.folder.engine.trees.check(), Ok(()));

        fs::hard_link(site.root.join("also"), site.root.join("third")).unwrap();
        settled(&mut a, &mut remote);
        fs::remove_file(site.root.join("third")).unwrap();
        settled(&mut a, &mut remote);
        let linked = server_nodes(&remote);
        assert!(
            live(&linked, &file) && live(&linked, &also),
            "{made}: {linked:?}"
        );
        assert_eq!(a.folder.engine.trees.check(), Ok(()));

        fs::remove_file(site.root.join("also")).unwrap();
        settled(&mut a, &mut remote);
        let after = server_nodes(&remote);
        assert!(live(&after, &file), "{made}: {after:?}");
        assert_eq!(fs::read(site.root.join("ignored/file")).unwrap(), b"kept");
        assert_eq!(a.folder.engine.trees.check(), Ok(()));

        drop(a);
        let mut a = site.start();
        a.connect(&remote);
        a.scan_until(|a| a.folder.scanned);
        settled(&mut a, &mut remote);
        assert_eq!(server_nodes(&remote), after, "{made}");
    }
}

#[test]
fn a_link_moved_into_an_ignored_folder_while_another_link_is_renamed_keeps_its_node() {
    for (made, link) in [("source/file", "also"), ("also", "source/file")] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        let settled = |a: &mut Device, remote: &mut Remote| {
            for _ in 0..3 {
                a.settle();
                a.run(remote);
            }
        };
        a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
        fs::create_dir(site.root.join("source")).unwrap();
        a.create(&mut remote, &site.root.join(made), b"kept");
        fs::hard_link(site.root.join(made), site.root.join(link)).unwrap();
        settled(&mut a, &mut remote);
        let before = server_nodes(&remote);
        let (file, also) = (live_id(&before, "file"), live_id(&before, "also"));
        let live =
            |nodes: &Nodes, id: &str| nodes.iter().any(|(_, node, live)| node == id && *live);

        fs::create_dir(site.root.join("ignored")).unwrap();
        fs::rename(site.root.join("also"), site.root.join("renamed")).unwrap();
        fs::rename(
            site.root.join("source/file"),
            site.root.join("ignored/file"),
        )
        .unwrap();
        settled(&mut a, &mut remote);
        let moved = server_nodes(&remote);
        assert!(live(&moved, &file), "{made}: {moved:?}");
        assert_eq!(live_id(&moved, "renamed"), also, "{made}: {moved:?}");
        assert_eq!(a.folder.engine.trees.check(), Ok(()));
    }
}

#[test]
fn a_file_moved_over_another_with_an_older_link_in_an_ignored_folder_leaves_the_server() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let settled = |a: &mut Device, remote: &mut Remote| {
        for _ in 0..3 {
            a.settle();
            a.run(remote);
        }
    };
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    a.create(&mut remote, &site.root.join("a"), b"kept");
    a.create(&mut remote, &site.root.join("x"), b"other");
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::hard_link(site.root.join("a"), site.root.join("ignored/l")).unwrap();
    settled(&mut a, &mut remote);

    fs::rename(site.root.join("a"), site.root.join("x")).unwrap();
    settled(&mut a, &mut remote);
    let after = server_nodes(&remote);
    assert!(
        !after.iter().any(|(name, _, live)| name == "a" && *live),
        "{after:?}"
    );
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

#[test]
fn a_file_moved_over_another_out_of_a_removed_folder_with_an_older_link_in_an_ignored_folder_leaves_the_server()
 {
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
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::hard_link(site.root.join("d/f"), site.root.join("ignored/l")).unwrap();
    settled(&mut a, &mut remote);

    fs::rename(site.root.join("d/f"), site.root.join("x")).unwrap();
    fs::remove_dir(site.root.join("d")).unwrap();
    settled(&mut a, &mut remote);
    let after = server_nodes(&remote);
    assert!(
        !(after.iter()).any(|(name, _, live)| (name == "d" || name == "f") && *live),
        "{after:?}"
    );
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

#[test]
fn removing_an_excluded_folder_lets_the_file_parked_behind_it_take_its_name() {
    let (mut remote, site, mut b, parked) = a_file_parked_behind_an_excluded_folder();
    fs::remove_dir_all(site.root.join("a/ß")).unwrap();
    for _ in 0..3 {
        b.settle();
        b.run(&mut remote);
    }
    assert_eq!(fs::read(site.root.join("a/ß")).unwrap(), b"moved on a");
    assert!(!parked.exists());
    assert_eq!(b.folder.view().problem, None);
    assert!(b.sent.iter().all(|commit| commit.ops.is_empty()));
    assert_eq!(remote.names(), ["a", "ß"]);
}

#[test]
fn an_excluded_folder_removed_while_the_device_is_stopped_frees_its_name_for_the_parked_file() {
    let (mut remote, site, b, parked) = a_file_parked_behind_an_excluded_folder();
    drop(b);
    fs::remove_dir_all(site.root.join("a/ß")).unwrap();
    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["ss/".into()],
    };
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    for _ in 0..3 {
        b.run(&mut remote);
        b.settle();
    }
    assert_eq!(fs::read(site.root.join("a/ß")).unwrap(), b"moved on a");
    assert!(!parked.exists());
    assert_eq!(b.folder.view().problem, None);
    assert!(b.sent.iter().all(|commit| commit.ops.is_empty()));
    assert_eq!(remote.names(), ["a", "ß"]);
}

#[test]
fn an_excluded_folder_moved_to_another_excluded_name_leaves_l_once_deleted() {
    an_excluded_folder_deleted_after_a_move_leaves_l("cache", ("cache", "other"), "other");
}

#[test]
fn an_excluded_folder_in_a_renamed_folder_leaves_l_once_deleted() {
    an_excluded_folder_deleted_after_a_move_leaves_l("sub/cache", ("sub", "moved"), "moved/cache");
}

fn an_excluded_folder_deleted_after_a_move_leaves_l(
    folder: &str,
    (from, to): (&str, &str),
    moved: &str,
) {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir_all(site.root.join(folder)).unwrap();
    b.create(
        &mut remote,
        &site.root.join(folder).join("file"),
        b"kept remotely",
    );
    drop(b);
    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["cache/".into(), "other/".into()],
    };
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    fs::rename(site.root.join(from), site.root.join(to)).unwrap();
    for _ in 0..3 {
        b.settle();
        b.run(&mut remote);
    }
    let name = moved.rsplit('/').next().unwrap();
    let held = |b: &Device| {
        (b.folder.engine.trees.local().iter()).any(|(_, node)| node.shape.name == name)
    };
    assert!(held(&b));
    fs::remove_dir_all(site.root.join(moved)).unwrap();
    for _ in 0..3 {
        b.settle();
        b.run(&mut remote);
    }
    assert!(!held(&b));
}
