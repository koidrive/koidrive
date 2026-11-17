// SPDX-License-Identifier: Apache-2.0

fn deleted_with_ignored_entries(
    synced: &[&str],
    ignored: &[&str],
) -> (Remote, Site, Device, Site, Device, String) {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    for path in synced {
        let path = site_a.root.join("app").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        a.create(&mut remote, &path, b"synced");
    }
    let app = live_id(&server_nodes(&remote), "app");
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&ignore_rules());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    for path in ignored {
        let path = site.root.join("app").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, local_only(&path)).unwrap();
    }
    b.settle();
    assert_eq!(b.state(), FolderState::UpToDate);

    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert!(b.work.is_empty());
    (remote, site_a, a, site, b, app)
}

fn ignore_rules() -> SyncRules {
    SyncRules {
        include: Vec::new(),
        exclude: vec![".env".into(), "*.log".into(), "node_modules/".into()],
        ..SyncRules::default()
    }
}

fn local_only(path: &Path) -> String {
    format!("local only: {}", path.file_name().unwrap().to_str().unwrap())
}

fn assert_kept(site: &Site, b: &Device, ignored: &[&str]) {
    for path in ignored {
        let path = site.root.join("app").join(path);
        assert_eq!(fs::read_to_string(&path).unwrap(), local_only(&path));
    }
    let trashed = trashed(&site.root).join(" ");
    for path in ignored {
        let name = Path::new(path).file_name().unwrap().to_str().unwrap();
        assert!(!trashed.contains(name), "{name} is in the trash: {trashed}");
    }
    assert!(b.folder.engine.effects.blocked().is_empty());
    let view = b.folder.view();
    assert_eq!((view.state, view.problem), (FolderState::UpToDate, None));
}

#[test]
fn a_remote_delete_trashes_the_synced_files_and_keeps_env_and_node_modules() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (remote, _, _, site, b, _) =
        deleted_with_ignored_entries(&["README.md", "src/main.rs"], &ignored);

    assert_kept(&site, &b, &ignored);
    assert!(!site.root.join("app/README.md").exists());
    assert!(!site.root.join("app/src").exists());
    assert_eq!(trashed(&site.root).len(), 3);
    assert!(b.sent.is_empty(), "nothing is sent for the kept folder");
    assert!(remote.names().is_empty());
}

#[test]
fn a_remote_delete_keeps_a_folder_whose_ignored_entries_sit_at_several_depths() {
    let ignored = ["sub/deeper/build.log", "other/.env", "sub/node_modules/x/y.js"];
    let synced = ["sub/a.txt", "sub/deeper/b.txt", "other/c.txt", "d.txt"];
    let (remote, _, _, site, b, _) = deleted_with_ignored_entries(&synced, &ignored);

    assert_kept(&site, &b, &ignored);
    for path in synced {
        assert!(!site.root.join("app").join(path).exists(), "{path}");
    }
    assert!(b.sent.is_empty());
    assert!(remote.names().is_empty());
}

#[test]
fn an_edit_beside_an_ignored_entry_survives_a_remote_delete_of_its_folder() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/notes.txt"), b"synced");
    a.create(&mut remote, &site_a.root.join("app/other.txt"), b"synced");
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&ignore_rules());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    fs::write(site.root.join("app/.env"), b"SECRET=local").unwrap();
    fs::write(site.root.join("app/notes.txt"), b"edited on b").unwrap();
    b.settle();

    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert_eq!(
        fs::read(site.root.join("app/notes.txt")).unwrap(),
        b"edited on b"
    );
    assert_eq!(fs::read(site.root.join("app/.env")).unwrap(), b"SECRET=local");
    assert!(!trashed(&site.root).join(" ").contains(".env"));
    let tree = remote.tree();
    assert!(tree.contains_key("app/notes.txt"), "{tree:?}");
    assert!(!tree.contains_key("app/.env"), "{tree:?}");
    let view = b.folder.view();
    assert_eq!((view.state, view.problem), (FolderState::UpToDate, None));
}

#[test]
fn a_synced_file_added_to_a_kept_folder_uploads_it_as_a_new_folder() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, _, _, site, mut b, old) =
        deleted_with_ignored_entries(&["README.md"], &ignored);

    fs::write(site.root.join("app/todo.txt"), b"written into the kept folder").unwrap();
    b.settle();
    b.run(&mut remote);
    b.settle();

    let tree = remote.tree();
    assert_eq!(
        tree.keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/todo.txt"]
    );
    assert_ne!(tree["app"], old, "a new folder, not the one another device deleted");
    let nodes = server_nodes(&remote);
    assert!(nodes.contains(&("app".into(), old, false)), "{nodes:?}");
    assert_kept(&site, &b, &ignored);
}

#[test]
fn a_kept_folder_takes_the_folder_another_device_makes_at_its_place() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, site, mut b, old) =
        deleted_with_ignored_entries(&["README.md"], &ignored);

    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    let app = live_id(&server_nodes(&remote), "app");
    assert_ne!(app, old);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert_eq!(
        fs::read(site.root.join("app/README.md")).unwrap(),
        b"cloned again"
    );
    assert!(b.sent.is_empty(), "the folder is taken, not created again: {:?}", b.sent);
    assert_eq!(local_named(&b, "app"), app);
    assert_eq!(remote.names(), ["README.md", "app"]);
    assert_kept(&site, &b, &ignored);
}

#[test]
fn a_restart_neither_uploads_nor_brings_back_a_kept_folder() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, _, _, site, b, old) = deleted_with_ignored_entries(&["README.md"], &ignored);
    drop(b);

    let mut b = site.start_with(&ignore_rules());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert!(remote.names().is_empty());
    assert!(server_nodes(&remote).contains(&("app".into(), old, false)));
    assert_kept(&site, &b, &ignored);
}

#[test]
fn a_kept_folder_the_person_renames_sends_nothing() {
    let ignored = [".env"];
    let (mut remote, _, _, site, mut b, _) = deleted_with_ignored_entries(&["README.md"], &ignored);

    fs::rename(site.root.join("app"), site.root.join("app-old")).unwrap();
    b.settle();
    b.run(&mut remote);

    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert!(remote.names().is_empty());
    let env = site.root.join("app-old/.env");
    assert_eq!(fs::read_to_string(&env).unwrap(), local_only(&env));
}

#[test]
fn a_kept_folder_whose_node_the_server_purges_stays_kept() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, _, _, site, mut b, old) =
        deleted_with_ignored_entries(&["README.md"], &ignored);

    let (_, readme, _) = (server_nodes(&remote).into_iter())
        .find(|(name, ..)| name == "README.md")
        .unwrap();
    assert!(remote.space.purge(&readme, NOW));
    assert!(remote.space.purge(&old, NOW));
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert!(b.folder.engine.trees.remote().get(&old).is_none());
    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert!(remote.names().is_empty());
    assert_kept(&site, &b, &ignored);
}

#[test]
fn a_file_another_device_makes_at_a_kept_folders_place_downloads_beside_it_under_a_conflict_name() {
    for name in ["app", "APP"] {
        let ignored = [".env"];
        let (mut remote, site_a, mut a, site, mut b, _) =
            deleted_with_ignored_entries(&["README.md"], &ignored);

        a.create(&mut remote, &site_a.root.join(name), b"a file now");
        for _ in 0..3 {
            b.pull(&remote);
            b.run(&mut remote);
            b.settle();
        }

        let names = remote.names();
        let [copy] = names.as_slice() else {
            panic!("{names:?}");
        };
        assert!(
            copy.starts_with(&format!("{name} (conflict from ")),
            "{copy}"
        );
        assert_eq!(fs::read(site.root.join(copy)).unwrap(), b"a file now");
        let env = site.root.join("app/.env");
        assert_eq!(fs::read_to_string(&env).unwrap(), local_only(&env));
        assert!(
            !remote.tree().keys().any(|path| path.ends_with(".env")),
            "{:?}",
            remote.tree()
        );
        let view = b.folder.view();
        assert_eq!(view.state, FolderState::UpToDate);
        assert_eq!(view.problem, None);
    }
}

#[test]
fn two_devices_that_keep_a_folder_rename_a_file_made_at_its_name_once() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"synced");
    let keepers: Vec<(Site, Device)> = ["b", "c"]
        .into_iter()
        .map(|name| {
            let site = Site::new(&remote, name);
            let mut device = site.start_with(&ignore_rules());
            device.connect(&remote);
            device.scan_until(|device| device.folder.scanned);
            device.rest(&mut remote);
            let env = site.root.join("app/.env");
            fs::write(&env, local_only(&env)).unwrap();
            device.settle();
            (site, device)
        })
        .collect();
    let [(site_b, mut b), (site_c, mut c)] = <[_; 2]>::try_from(keepers).ok().unwrap();
    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    for device in [&mut b, &mut c] {
        device.pull(&remote);
        device.rest(&mut remote);
    }

    a.create(&mut remote, &site_a.root.join("app"), b"a file now");
    b.pull(&remote);
    c.pull(&remote);
    b.settle();
    c.settle();
    b.run(&mut remote);
    c.run(&mut remote);
    for device in [&b, &c] {
        let renames = (device.sent.iter().flat_map(|commit| &commit.ops))
            .filter(|op| matches!(&op.op, Some(op::Op::Move(moved)) if moved.from.is_some()))
            .count();
        assert_eq!(renames, 1);
    }
    for _ in 0..3 {
        for device in [&mut a, &mut b, &mut c] {
            device.pull(&remote);
            device.rest(&mut remote);
        }
    }

    let names = remote.names();
    let [copy] = names.as_slice() else {
        panic!("{names:?}");
    };
    assert!(copy.starts_with("app (conflict from "), "{copy}");
    for (site, device) in [(&site_a, &a), (&site_b, &b), (&site_c, &c)] {
        assert_eq!(fs::read(site.root.join(copy)).unwrap(), b"a file now");
        let view = device.folder.view();
        assert_eq!((view.state, view.problem), (FolderState::UpToDate, None));
    }
    for site in [&site_b, &site_c] {
        let env = site.root.join("app/.env");
        assert_eq!(fs::read_to_string(&env).unwrap(), local_only(&env));
    }
    assert!(!site_a.root.join("app").exists());
    assert!(
        !remote.tree().keys().any(|path| path.ends_with(".env")),
        "{:?}",
        remote.tree()
    );
}

#[test]
fn a_restart_after_renaming_a_file_made_at_a_kept_folders_name_downloads_it_without_renaming_again()
{
    let ignored = [".env"];
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_ignored_entries(&["README.md"], &ignored);
    a.create(&mut remote, &site_a.root.join("app"), b"a file now");
    b.pull(&remote);
    b.settle();
    let Some(Work::Send(commit)) = b.work.pop_front() else {
        panic!("b sends the rename first");
    };
    remote.commit(&b.caller, &commit).unwrap();
    let names = remote.names();
    let [copy] = names.as_slice() else {
        panic!("{names:?}");
    };
    drop(b);

    let mut b = site.start_with(&ignore_rules());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    for _ in 0..3 {
        b.pull(&remote);
        b.rest(&mut remote);
    }

    assert_eq!(remote.names(), names);
    assert_eq!(fs::read(site.root.join(copy)).unwrap(), b"a file now");
    assert_kept(&site, &b, &ignored);
}

#[test]
fn a_rule_that_lets_a_kept_entry_in_uploads_the_kept_folder_as_a_new_folder() {
    let ignored = ["build.log"];
    let (mut remote, _, _, site, b, old) = deleted_with_ignored_entries(&["README.md"], &ignored);
    drop(b);

    let mut b = site.start_with(&SyncRules {
        include: Vec::new(),
        exclude: vec![".env".into()],
        ..SyncRules::default()
    });
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    let tree = remote.tree();
    assert_eq!(
        tree.keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/build.log"]
    );
    assert_ne!(tree["app"], old);
}

#[test]
fn a_kept_folder_moves_with_the_folder_another_device_renames_around_it() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site_a.root.join("proj/app")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/notes.txt"), b"synced");
    a.create(&mut remote, &site_a.root.join("proj/app/README.md"), b"synced");
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&ignore_rules());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    fs::write(site.root.join("proj/app/.env"), b"SECRET=local").unwrap();
    b.settle();

    fs::remove_dir_all(site_a.root.join("proj/app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    fs::rename(site_a.root.join("proj"), site_a.root.join("renamed")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert_eq!(
        fs::read(site.root.join("renamed/app/.env")).unwrap(),
        b"SECRET=local"
    );
    assert_eq!(fs::read(site.root.join("renamed/notes.txt")).unwrap(), b"synced");
    assert!(!site.root.join("proj").exists());
    assert!(!site.root.join("renamed/app/README.md").exists());
    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["renamed", "renamed/notes.txt"]
    );
    assert!(b.folder.engine.effects.blocked().is_empty());
    let view = b.folder.view();
    assert_eq!((view.state, view.problem), (FolderState::UpToDate, None));
}

fn deleted_with_rules_files(
    rules: &[(&str, &str)],
    synced: &[&str],
    ignored: &[&str],
) -> (Remote, Site, Device, Site, Device, String) {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let files = (rules.iter().map(|(path, rules)| (*path, rules.as_bytes())))
        .chain(synced.iter().map(|path| (*path, b"synced".as_slice())));
    for (path, content) in files {
        let path = site_a.root.join("app").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        a.create(&mut remote, &path, content);
    }
    let app = live_id(&server_nodes(&remote), "app");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    for _ in 0..3 {
        b.run(&mut remote);
        b.settle();
    }
    for path in ignored {
        let path = site.root.join("app").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, local_only(&path)).unwrap();
    }
    b.settle();
    b.run(&mut remote);
    assert_eq!(b.state(), FolderState::UpToDate);
    assert!(b.sent.is_empty(), "{:?}", b.sent);

    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    (remote, site_a, a, site, b, app)
}

#[test]
fn a_kept_folder_whose_rules_file_was_synced_uploads_nothing() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/.koiignore"), b".env\nnode_modules/\n");
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"synced");
    let old = live_id(&server_nodes(&remote), "app");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    fs::write(site.root.join("app/.env"), b"SECRET=local").unwrap();
    fs::create_dir_all(site.root.join("app/node_modules/pkg")).unwrap();
    fs::write(site.root.join("app/node_modules/pkg/index.js"), b"built").unwrap();
    b.settle();
    b.run(&mut remote);
    assert!(!remote.tree().contains_key("app/.env"));

    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    let tree = remote.tree();
    assert!(!tree.contains_key("app/.env"), "{tree:?}");
    assert!(!server_nodes(&remote).contains(&("app".into(), old, true)), "{tree:?}");
}

#[test]
fn a_kept_folder_keeps_the_rules_files_that_leave_its_entries_out_at_any_depth() {
    let rules = [(".koiignore", ".env\n"), ("sub/.koiignore", "build/\n")];
    let ignored = [".env", "sub/build/out.o"];
    let (remote, _, _, site, b, _) =
        deleted_with_rules_files(&rules, &["README.md", "sub/a.txt"], &ignored);

    assert_kept(&site, &b, &ignored);
    for (path, rules) in rules {
        assert_eq!(fs::read_to_string(site.root.join("app").join(path)).unwrap(), rules);
    }
    assert!(!site.root.join("app/README.md").exists());
    assert!(!site.root.join("app/sub/a.txt").exists());
    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert!(remote.names().is_empty(), "{:?}", remote.tree());
}

#[test]
fn a_remote_delete_of_a_folder_whose_rules_file_leaves_nothing_out_here_removes_it_whole() {
    let rules = [(".koiignore", ".env\n"), ("sub/.koiignore", "build/\n")];
    let (remote, _, _, site, b, _) = deleted_with_rules_files(&rules, &["README.md"], &[]);

    assert!(!site.root.join("app").exists());
    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert!(remote.names().is_empty());
    assert!(b.folder.engine.effects.blocked().is_empty());
    let view = b.folder.view();
    assert_eq!((view.state, view.problem), (FolderState::UpToDate, None));
}

#[test]
fn an_edit_to_a_kept_rules_file_that_lets_its_entries_in_uploads_the_folder_as_new() {
    let (mut remote, _, _, site, mut b, old) =
        deleted_with_rules_files(&[(".koiignore", ".env\n")], &["README.md"], &[".env"]);

    fs::write(site.root.join("app/.koiignore"), b"").unwrap();
    for _ in 0..2 {
        b.settle();
        b.run(&mut remote);
    }

    let tree = remote.tree();
    assert_eq!(
        tree.keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/.env", "app/.koiignore"]
    );
    assert_ne!(tree["app"], old);
}

#[test]
fn a_kept_folder_with_its_rules_file_takes_the_folder_another_device_makes_at_its_place() {
    let rules = ".env\nnode_modules/\n";
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, site, mut b, old) =
        deleted_with_rules_files(&[(".koiignore", rules)], &["README.md"], &ignored);

    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/.koiignore"), rules.as_bytes());
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    let app = live_id(&server_nodes(&remote), "app");
    assert_ne!(app, old);
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    assert_eq!(local_named(&b, "app"), app);
    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/.koiignore", "app/README.md"]
    );
    assert_eq!(
        names(&site.root.join("app")),
        [".env", ".koiignore", "README.md", "node_modules"]
    );
    assert_kept(&site, &b, &ignored);
}

#[test]
fn a_kept_folder_takes_the_folder_another_device_makes_at_its_name_in_another_case() {
    let ignored = [".env"];
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_ignored_entries(&["README.md"], &ignored);

    fs::create_dir(site_a.root.join("App")).unwrap();
    a.create(&mut remote, &site_a.root.join("App/README.md"), b"cloned again");
    let app = live_id(&server_nodes(&remote), "App");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["App", "App/README.md"]
    );
    let folder = (names(&site.root).into_iter())
        .find(|name| name.eq_ignore_ascii_case("app"))
        .unwrap();
    assert_eq!(local_named(&b, &folder), app);
    let env = site.root.join(&folder).join(".env");
    assert_eq!(fs::read_to_string(&env).unwrap(), local_only(&env));
    let view = b.folder.view();
    assert_eq!((view.state, view.problem), (FolderState::UpToDate, None));
}

#[test]
fn a_kept_folder_takes_a_folder_made_at_its_place_with_other_rules_without_a_conflict_copy() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, _site, mut b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env\nnode_modules/\n")],
        &["README.md"],
        &ignored,
    );

    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/.koiignore"), b".env\nnode_modules/\n*.log\n");
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/.koiignore", "app/README.md"]
    );
}

#[test]
fn a_kept_rules_file_the_person_edited_stays_beside_the_rules_of_a_folder_made_at_its_place() {
    let ignored = [".env"];
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_rules_files(&[(".koiignore", ".env\n")], &["README.md"], &ignored);
    fs::write(site.root.join("app/.koiignore"), b".env\n*.log\n").unwrap();
    b.settle();
    b.run(&mut remote);
    assert!(b.sent.is_empty(), "{:?}", b.sent);

    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/.koiignore"), b".env\nbuild/\n");
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    let tree = remote.tree();
    let names: Vec<&str> = tree.keys().map(String::as_str).collect();
    assert_eq!(names.len(), 4, "{names:?}");
    assert!(names[2].starts_with("app/.koiignore (conflict from b"), "{names:?}");
    assert!(!tree.contains_key("app/.env"), "{names:?}");
    assert_eq!(
        fs::read(site.root.join("app/.koiignore")).unwrap(),
        b".env\nbuild/\n"
    );
    let copy = site.root.join("app").join(&names[2]["app/".len()..]);
    assert_eq!(fs::read(copy).unwrap(), b".env\n*.log\n");
}

#[test]
fn a_kept_folder_whose_rules_sit_in_the_deleted_folder_above_it_sends_nothing() {
    let ignored = ["sub/.env", "sub/node_modules/pkg/index.js"];
    let (remote, _, _, site, b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env\nnode_modules/\n")],
        &["README.md", "sub/README.md"],
        &ignored,
    );

    assert_kept(&site, &b, &ignored);
    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert!(remote.names().is_empty(), "{:?}", remote.tree());
}

fn synced_with_rules_file(remote: &mut Remote) -> (Site, Device, Site, Device) {
    let (site_a, mut a) = linked(remote, "a");
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(remote, &site_a.root.join("app/.koiignore"), b".env\n");
    a.create(remote, &site_a.root.join("app/README.md"), b"synced");
    let site = Site::new(remote, "b");
    let mut b = site.start();
    b.connect(remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(remote);
    b.settle();
    fs::write(site.root.join("app/.env"), b"SECRET=local").unwrap();
    b.settle();
    b.run(remote);
    assert!(!remote.tree().contains_key("app/.env"));
    (site_a, a, site, b)
}

#[test]
fn an_edit_that_brings_back_a_deleted_folder_keeps_its_rules_in_force() {
    let mut remote = Remote::new();
    let (site_a, mut a, site, mut b) = synced_with_rules_file(&mut remote);
    fs::write(site.root.join("app/README.md"), b"edited on b").unwrap();
    b.settle();
    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    for _ in 0..4 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    let tree = remote.tree();
    assert_eq!(
        tree.keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/.koiignore", "app/README.md"]
    );
    assert_eq!(fs::read(site.root.join("app/.env")).unwrap(), b"SECRET=local");
    assert_eq!(fs::read(site.root.join("app/.koiignore")).unwrap(), b".env\n");
}

#[test]
fn a_file_the_scans_have_not_seen_that_brings_back_a_deleted_folder_keeps_its_rules_in_force() {
    let mut remote = Remote::new();
    let (site_a, mut a, site, mut b) = synced_with_rules_file(&mut remote);
    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    fs::write(site.root.join("app/new.txt"), b"not seen yet").unwrap();
    for _ in 0..4 {
        b.run(&mut remote);
        b.settle();
        b.pull(&remote);
    }

    let tree = remote.tree();
    assert_eq!(
        tree.keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/.koiignore", "app/new.txt"]
    );
    assert_eq!(fs::read(site.root.join("app/.env")).unwrap(), b"SECRET=local");
    assert_eq!(fs::read(site.root.join("app/.koiignore")).unwrap(), b".env\n");
}

#[test]
fn a_kept_folder_taken_over_by_a_folder_without_rules_uploads_none_of_its_entries() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, _site, mut b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env\nnode_modules/\n")],
        &["README.md"],
        &ignored,
    );

    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    let tree = remote.tree();
    assert_eq!(tree.keys().collect::<Vec<_>>(), ["app", "app/README.md"], "{tree:?}");
}

#[test]
fn a_kept_folder_taken_over_while_this_device_is_stopped_uploads_none_of_its_entries() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, site, b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env\nnode_modules/\n")],
        &["README.md"],
        &ignored,
    );
    drop(b);

    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/README.md"]
    );
    for path in ignored {
        let path = site.root.join("app").join(path);
        assert_eq!(fs::read_to_string(&path).unwrap(), local_only(&path));
    }
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn an_entry_kept_through_a_takeover_syncs_only_once_the_person_makes_it_again() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, site, mut b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env\nnode_modules/\n")],
        &["README.md"],
        &ignored,
    );
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    fs::write(site.root.join("app/.env"), b"SECRET=edited").unwrap();
    fs::write(site.root.join("app/node_modules/pkg/new.js"), b"new").unwrap();
    fs::write(site.root.join("app/notes.txt"), b"new").unwrap();
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/README.md", "app/notes.txt"]
    );
    let mut kept = b.folder.view().kept_entries;
    kept.sort();
    assert_eq!(kept, [koidrive_fs::ItemPath::from("app/.env"), "app/node_modules".into()]);

    fs::remove_file(site.root.join("app/.env")).unwrap();
    b.settle();
    b.run(&mut remote);
    fs::write(site.root.join("app/.env"), b"PUBLIC=yes").unwrap();
    b.settle();
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/.env", "app/README.md", "app/notes.txt"]
    );
    assert_eq!(b.folder.view().kept_entries, [koidrive_fs::ItemPath::from("app/node_modules")]);

    fs::remove_dir_all(site.root.join("app/node_modules")).unwrap();
    b.settle();
    b.run(&mut remote);
    assert!(b.folder.view().kept_entries.is_empty());
    assert!(b.folder.engine.trees.state().kept_entries.is_empty());
}


#[test]
fn a_kept_folder_below_a_folder_taken_over_without_rules_uploads_none_of_its_entries() {
    let ignored = ["sub/.env", "sub/node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, site, mut b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env\nnode_modules/\n")],
        &["README.md", "sub/README.md"],
        &ignored,
    );

    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/README.md"]
    );
    for path in ignored {
        let path = site.root.join("app").join(path);
        assert_eq!(fs::read_to_string(&path).unwrap(), local_only(&path));
    }
}


#[test]
fn a_kept_rules_file_the_person_renames_uploads_under_a_new_id() {
    let (mut remote, _, _, site, mut b, _) =
        deleted_with_rules_files(&[(".koiignore", ".env\n")], &["README.md"], &[".env"]);
    let nodes = server_nodes(&remote);
    let old = nodes.iter().find(|(name, ..)| name == ".koiignore").unwrap().1.clone();

    fs::rename(site.root.join("app/.koiignore"), site.root.join("app/notes.txt")).unwrap();
    for _ in 0..3 {
        b.settle();
        b.run(&mut remote);
    }

    let tree = remote.tree();
    assert_eq!(
        tree.keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/.env", "app/notes.txt"]
    );
    assert_ne!(tree["app/notes.txt"], old);
}


#[test]
fn an_entry_made_in_a_kept_folder_before_a_takeover_stays_out_of_sync() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, site, mut b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env*\nnode_modules/\n")],
        &["README.md"],
        &ignored,
    );
    let made = site.root.join("app/.env.local");
    fs::write(&made, local_only(&made)).unwrap();
    b.settle();
    b.run(&mut remote);

    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    fs::write(&made, b"SECRET=edited").unwrap();
    for _ in 0..2 {
        b.settle();
        b.run(&mut remote);
    }

    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/README.md"]
    );
    assert_eq!(fs::read(&made).unwrap(), b"SECRET=edited");
    let mut kept = b.folder.view().kept_entries;
    kept.sort();
    assert_eq!(
        kept,
        [
            koidrive_fs::ItemPath::from("app/.env"),
            "app/.env.local".into(),
            "app/node_modules".into()
        ]
    );
}

#[test]
fn a_kept_folder_taken_over_by_a_folder_with_its_own_rules_keeps_its_entries_out_and_follows_the_new_rules() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, site, mut b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env*\nnode_modules/\n")],
        &["README.md"],
        &ignored,
    );

    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/.koiignore"), b"*.log\n");
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert_eq!(fs::read(site.root.join("app/.koiignore")).unwrap(), b"*.log\n");

    fs::write(site.root.join("app/.env"), b"SECRET=edited").unwrap();
    fs::write(site.root.join("app/.env.example"), b"SECRET=").unwrap();
    fs::write(site.root.join("app/debug.log"), b"log").unwrap();
    b.settle();
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/.env.example", "app/.koiignore", "app/README.md"]
    );
    let mut kept = b.folder.view().kept_entries;
    kept.sort();
    assert_eq!(kept, [koidrive_fs::ItemPath::from("app/.env"), "app/node_modules".into()]);
}


#[test]
fn a_kept_entry_the_person_renames_stays_out_of_sync_and_keeps_its_name() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, site, mut b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env\nnode_modules/\n")],
        &["README.md"],
        &ignored,
    );
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    let settle = |b: &mut Device, remote: &mut Remote| {
        for _ in 0..2 {
            b.settle();
            b.run(remote);
        }
    };

    fs::rename(site.root.join("app/.env"), site.root.join("app/settings.txt")).unwrap();
    settle(&mut b, &mut remote);
    assert_eq!(
        b.folder.view().kept_entries,
        [koidrive_fs::ItemPath::from("app/node_modules"), "app/settings.txt".into()]
    );
    fs::write(site.root.join("app/.env"), b"SECRET=new").unwrap();
    settle(&mut b, &mut remote);
    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/README.md"]
    );
    assert_eq!(
        b.folder.view().kept_entries,
        [
            koidrive_fs::ItemPath::from("app/.env"),
            "app/node_modules".into(),
            "app/settings.txt".into()
        ]
    );

    fs::remove_file(site.root.join("app/settings.txt")).unwrap();
    settle(&mut b, &mut remote);
    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/README.md"]
    );
    fs::remove_file(site.root.join("app/.env")).unwrap();
    settle(&mut b, &mut remote);
    fs::write(site.root.join("app/.env"), b"PUBLIC=yes").unwrap();
    settle(&mut b, &mut remote);
    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/.env", "app/README.md"]
    );
    assert_eq!(b.folder.view().kept_entries, [koidrive_fs::ItemPath::from("app/node_modules")]);
}

#[test]
fn a_save_that_renames_a_kept_entry_to_a_backup_uploads_neither_file() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, site, mut b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env\nnode_modules/\n")],
        &["README.md"],
        &ignored,
    );
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    fs::rename(site.root.join("app/.env"), site.root.join("app/.env~")).unwrap();
    fs::write(site.root.join("app/.env"), b"SECRET=saved").unwrap();
    for _ in 0..2 {
        b.settle();
        b.run(&mut remote);
    }
    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    for _ in 0..2 {
        b.settle();
        b.run(&mut remote);
    }

    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["app", "app/README.md"]
    );
    assert_eq!(
        b.folder.view().kept_entries,
        [
            koidrive_fs::ItemPath::from("app/.env"),
            "app/.env~".into(),
            "app/node_modules".into()
        ]
    );
}


#[test]
fn a_kept_entry_moved_out_of_its_folder_stays_out_of_sync_after_the_folder_is_deleted() {
    for deleted_by in ["this device", "another device"] {
        let ignored = [".env", "node_modules/pkg/index.js"];
        let (mut remote, site_a, mut a, site, mut b, _) = deleted_with_rules_files(
            &[(".koiignore", ".env\nnode_modules/\n")],
            &["README.md"],
            &ignored,
        );
        fs::create_dir(site_a.root.join("app")).unwrap();
        a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
        for _ in 0..3 {
            b.pull(&remote);
            b.run(&mut remote);
            b.settle();
        }

        fs::rename(site.root.join("app/.env"), site.root.join(".env")).unwrap();
        fs::remove_dir_all(site.root.join("app/node_modules")).unwrap();
        for _ in 0..2 {
            b.settle();
            b.run(&mut remote);
        }
        match deleted_by {
            "this device" => fs::remove_dir_all(site.root.join("app")).unwrap(),
            _ => {
                fs::remove_dir_all(site_a.root.join("app")).unwrap();
                a.scan_until(|a| !a.work.is_empty());
                a.run(&mut remote);
            }
        }
        for _ in 0..3 {
            b.pull(&remote);
            b.run(&mut remote);
            b.settle();
        }
        fs::write(site.root.join(".env"), b"SECRET=edited").unwrap();
        for _ in 0..2 {
            b.settle();
            b.run(&mut remote);
        }

        assert!(!site.root.join("app").exists(), "{deleted_by}");
        assert!(!remote.tree().contains_key("app"), "{deleted_by}: {:?}", remote.tree());
        assert!(!remote.tree().contains_key(".env"), "{deleted_by}: {:?}", remote.tree());
        assert_eq!(b.folder.view().kept_entries, [koidrive_fs::ItemPath::from(".env")], "{deleted_by}");
    }
}

#[test]
fn a_kept_folder_deleted_again_after_a_takeover_without_rules_takes_the_next_folder_made_at_its_place() {
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_rules_files(&[(".koiignore", ".env\n")], &["README.md"], &[".env"]);
    for round in ["cloned again", "cloned a third time"] {
        fs::create_dir(site_a.root.join("app")).unwrap();
        a.create(&mut remote, &site_a.root.join("app/README.md"), round.as_bytes());
        for _ in 0..3 {
            b.pull(&remote);
            b.run(&mut remote);
            b.settle();
        }
        assert_eq!(fs::read(site.root.join("app/README.md")).unwrap(), round.as_bytes());
        assert!(site.root.join("app/.env").exists(), "{round}: {:?}", b.folder.view().problem);
        assert!(!remote.tree().contains_key("app/.env"), "{round}: {:?}", remote.tree());
        if round == "cloned again" {
            fs::remove_dir_all(site_a.root.join("app")).unwrap();
            a.scan_until(|a| !a.work.is_empty());
            a.run(&mut remote);
            for _ in 0..3 {
                b.pull(&remote);
                b.run(&mut remote);
                b.settle();
            }
            assert!(site.root.join("app/.env").exists(), "{:?}", b.folder.view().problem);
            assert!(nothing_parked(&site), "{:?}", files(&site.root));
        }
    }
    assert_eq!(b.folder.view().problem, None);
    assert!(nothing_parked(&site), "{:?}", files(&site.root));
}

#[test]
fn a_kept_entry_moved_into_a_folder_another_device_deletes_and_makes_again_stays_out_of_sync() {
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_rules_files(&[(".koiignore", ".env\n")], &["README.md"], &[".env"]);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    fs::create_dir(site_a.root.join("other")).unwrap();
    a.create(&mut remote, &site_a.root.join("other/notes.md"), b"notes");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }

    fs::rename(site.root.join("app/.env"), site.root.join("other/.env")).unwrap();
    for _ in 0..2 {
        b.settle();
        b.run(&mut remote);
    }
    fs::remove_dir_all(site_a.root.join("other")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert!(site.root.join("other/.env").exists(), "{:?}", b.folder.view().problem);

    fs::create_dir(site_a.root.join("other")).unwrap();
    a.create(&mut remote, &site_a.root.join("other/notes.md"), b"made again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert_eq!(fs::read(site.root.join("other/notes.md")).unwrap(), b"made again");
    fs::write(site.root.join("other/.env"), b"SECRET=after the takeover").unwrap();
    for _ in 0..2 {
        b.settle();
        b.run(&mut remote);
    }
    assert_eq!(fs::read(site.root.join("other/.env")).unwrap(), b"SECRET=after the takeover");
    assert!(nothing_parked(&site), "{:?}", files(&site.root));
    assert!(!remote.tree().contains_key("other/.env"), "{:?}", remote.tree());
    assert_eq!(b.folder.view().problem, None);
    assert_eq!(b.folder.view().kept_entries, [koidrive_fs::ItemPath::from("other/.env")]);
}

fn nothing_parked(site: &Site) -> bool {
    match fs::read_dir(site.root.join(".koidrive/parked")) {
        Ok(mut entries) => entries.next().is_none(),
        Err(err) => err.kind() == std::io::ErrorKind::NotFound || panic!("{err}"),
    }
}

fn taken_over_with_rules(rules: &[u8]) -> (Remote, Site, Device, Site, Device) {
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_rules_files(&[(".koiignore", ".env\n")], &["README.md"], &[".env"]);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/.koiignore"), rules);
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert_eq!(fs::read(site.root.join("app/.koiignore")).unwrap(), rules);
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
    (remote, site_a, a, site, b)
}

fn settled(b: &mut Device, remote: &mut Remote) {
    for _ in 0..3 {
        b.settle();
        b.run(remote);
    }
}

#[test]
fn a_kept_entry_syncs_once_the_person_edits_the_rules_that_left_it_out_to_let_it_in() {
    let (mut remote, _, _, site, mut b) = taken_over_with_rules(b".env\n");
    assert_eq!(b.folder.view().kept_entries, [koidrive_fs::ItemPath::from("app/.env")]);

    fs::write(site.root.join("app/.koiignore"), b".env\n*.log\n").unwrap();
    settled(&mut b, &mut remote);
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
    assert_eq!(b.folder.view().kept_entries, [koidrive_fs::ItemPath::from("app/.env")]);

    fs::write(site.root.join("app/.koiignore"), b"*.log\n").unwrap();
    settled(&mut b, &mut remote);
    assert!(remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
    assert!(b.folder.view().kept_entries.is_empty(), "{:?}", b.folder.view().kept_entries);
}

#[test]
fn a_kept_entry_stays_out_of_sync_when_the_rules_change_without_the_person() {
    let (mut remote, site_a, mut a, site, mut b) = taken_over_with_rules(b".env\n");
    fs::write(site_a.root.join("app/.koiignore"), b"*.log\n").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert_eq!(fs::read(site.root.join("app/.koiignore")).unwrap(), b"*.log\n");
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());

    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    settled(&mut b, &mut remote);
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
    assert_eq!(b.folder.view().kept_entries, [koidrive_fs::ItemPath::from("app/.env")]);
}

#[test]
fn a_downloaded_negation_does_not_release_a_kept_entry_beside_an_unsent_edit_of_the_rules_that_left_it_out()
 {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join(".koiignore"), b".env\n");
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"synced");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    settled(&mut b, &mut remote);
    fs::write(site.root.join("app/.env"), b"SECRET=b").unwrap();
    settled(&mut b, &mut remote);
    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/.koiignore"), b"*.tmp\n");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert_eq!(
        b.folder.view().kept_entries,
        [koidrive_fs::ItemPath::from("app/.env")]
    );

    fs::write(site_a.root.join("app/.koiignore"), b"*.tmp\n!.env\n").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    fs::write(site.root.join(".koiignore"), b".env\n*.bak\n").unwrap();
    b.pull(&remote);
    for _ in 0..4 {
        b.run(&mut remote);
        b.settle();
    }
    assert!(
        !remote.tree().contains_key("app/.env"),
        "{:?}",
        remote.tree()
    );
}

#[test]
fn a_kept_entry_syncs_once_the_person_lets_it_in_with_a_rules_edit_made_while_stopped() {
    let (mut remote, _, _, site, b) = taken_over_with_rules(b".env\n");
    drop(b);
    fs::write(site.root.join("app/.koiignore"), b"*.log\n").unwrap();

    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    settled(&mut b, &mut remote);
    assert!(
        remote.tree().contains_key("app/.env"),
        "{:?}",
        remote.tree()
    );
    assert!(
        b.folder.view().kept_entries.is_empty(),
        "{:?}",
        b.folder.view().kept_entries
    );
}

#[test]
fn a_kept_entry_syncs_once_the_person_lets_it_in_with_local_lists_changed_while_stopped() {
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_ignored_entries(&["README.md"], &[".env"]);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(
        &mut remote,
        &site_a.root.join("app/README.md"),
        b"cloned again",
    );
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert_eq!(
        b.folder.view().kept_entries,
        [koidrive_fs::ItemPath::from("app/.env")]
    );
    drop(b);

    let mut b = site.start_with(&SyncRules {
        exclude: vec!["*.log".into()],
        ..SyncRules::default()
    });
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    settled(&mut b, &mut remote);
    assert!(
        remote.tree().contains_key("app/.env"),
        "{:?}",
        remote.tree()
    );
    assert!(
        b.folder.view().kept_entries.is_empty(),
        "{:?}",
        b.folder.view().kept_entries
    );
}

#[test]
fn another_devices_file_at_a_kept_entrys_name_is_reported_and_the_kept_entry_stays_here() {
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_rules_files(&[(".koiignore", ".env\n")], &["README.md"], &[".env"]);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    let kept = fs::read_to_string(site.root.join("app/.env")).unwrap();

    a.create(&mut remote, &site_a.root.join("app/.env"), b"SHARED=from a");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    let problem = b.folder.view().problem.unwrap_or_default();
    assert!(problem.contains("\"app/.env\" (its name here is taken"), "{problem}");
    assert_eq!(fs::read_to_string(site.root.join("app/.env")).unwrap(), kept);
    assert_eq!(b.folder.view().kept_entries, [koidrive_fs::ItemPath::from("app/.env")]);
    assert!(nothing_parked(&site), "{:?}", files(&site.root));
    for _ in 0..3 {
        a.pull(&remote);
        a.run(&mut remote);
        a.settle();
    }
    assert_eq!(fs::read_to_string(site_a.root.join("app/.env")).unwrap(), "SHARED=from a");
    let on_a = files(&site_a.root);
    assert!(on_a.iter().all(|(_, content)| *content != kept), "{on_a:?}");
}


#[test]
fn a_kept_folder_inside_a_kept_folder_takes_the_folder_another_device_makes_at_its_place() {
    let (mut remote, site_a, mut a, site, mut b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env\n")],
        &["README.md", "sub/lib.rs"],
        &["sub/.env"],
    );
    assert!(site.root.join("app/sub/.env").exists());
    fs::create_dir_all(site_a.root.join("app/sub")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/sub/lib.rs"), b"made again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    let tree = remote.tree();
    assert!(!tree.keys().any(|k| k.contains("conflict") || k.ends_with(".env")), "{tree:?} {:?}", files(&site.root));
    assert_eq!(fs::read(site.root.join("app/sub/lib.rs")).unwrap(), b"made again");
    assert!(site.root.join("app/sub/.env").exists(), "{:?}", files(&site.root));
}

#[test]
fn a_kept_entry_syncs_once_the_person_lets_it_in_while_another_devices_new_rules_file_lands() {
    let (mut remote, site_a, mut a, site, mut b) = taken_over_with_rules(b".env\n");
    a.create(&mut remote, &site_a.root.join(".koiignore"), b"*.tmp\n");
    b.pull(&remote);
    b.run(&mut remote);
    fs::write(site.root.join("app/.koiignore"), b"*.log\n").unwrap();
    settled(&mut b, &mut remote);
    assert!(
        remote.tree().contains_key("app/.env"),
        "{:?}",
        remote.tree()
    );
}

#[test]
fn a_kept_entry_syncs_once_the_person_renames_the_rules_that_left_it_out_away() {
    let (mut remote, _, _, site, mut b) = taken_over_with_rules(b".env\n");
    fs::rename(
        site.root.join("app/.koiignore"),
        site.root.join("app/koiignore.off"),
    )
    .unwrap();
    settled(&mut b, &mut remote);
    assert!(
        remote.tree().contains_key("app/.env"),
        "{:?}",
        remote.tree()
    );
}

fn kept_by_root_rules() -> (Remote, Site, Device, Site, Device) {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join(".koiignore"), b".env\n");
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"synced");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    settled(&mut b, &mut remote);
    fs::write(site.root.join("app/.env"), b"SECRET=b").unwrap();
    settled(&mut b, &mut remote);
    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"again");
    pulled(&mut b, &mut remote);
    assert_eq!(
        b.folder.view().kept_entries,
        [koidrive_fs::ItemPath::from("app/.env")]
    );
    (remote, site_a, a, site, b)
}

fn pulled(b: &mut Device, remote: &mut Remote) {
    for _ in 0..3 {
        b.pull(remote);
        b.run(remote);
        b.settle();
    }
}

#[test]
fn a_kept_entry_stays_kept_when_the_person_edits_rules_another_device_changed_back_and_forth() {
    let (mut remote, site_a, mut a, site, mut b) = kept_by_root_rules();
    for rules in [b"*.tmp\n".as_slice(), b".env\n", b"*.tmp\n"] {
        fs::write(site_a.root.join(".koiignore"), rules).unwrap();
        a.scan_until(|a| !a.work.is_empty());
        a.run(&mut remote);
        pulled(&mut b, &mut remote);
    }
    assert!(
        !remote.tree().contains_key("app/.env"),
        "{:?}",
        remote.tree()
    );

    fs::write(site.root.join(".koiignore"), b"*.tmp\n*.bak\n").unwrap();
    settled(&mut b, &mut remote);
    settled(&mut b, &mut remote);

    assert!(
        !remote.tree().contains_key("app/.env"),
        "{:?}",
        remote.tree()
    );
}

#[test]
fn a_kept_entry_another_devices_negation_let_in_stays_kept_when_the_person_edits_that_rules_file() {
    let (mut remote, site_a, mut a, site, mut b) = kept_by_root_rules();
    a.create(&mut remote, &site_a.root.join("app/.koiignore"), b"!.env\n");
    pulled(&mut b, &mut remote);
    assert!(
        !remote.tree().contains_key("app/.env"),
        "{:?}",
        remote.tree()
    );

    fs::write(site.root.join("app/.koiignore"), b"!.env\n*.bak\n").unwrap();
    settled(&mut b, &mut remote);
    settled(&mut b, &mut remote);

    assert!(
        !remote.tree().contains_key("app/.env"),
        "{:?}",
        remote.tree()
    );
}

#[test]
fn a_viewer_keeping_a_folder_reports_a_file_made_at_its_name() {
    let (mut remote, site_a, mut a, _site, mut b, _) =
        deleted_with_ignored_entries(&["README.md"], &[".env"]);
    b.caller.role = Role::Viewer;
    b.connect(&remote);
    b.pull(&remote);
    assert_eq!(b.folder.engine.commits.halted(), Some(CommitHalt::ReadOnly));

    a.create(&mut remote, &site_a.root.join("app"), b"a file now");
    for _ in 0..8 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    let view = b.folder.view();
    assert_eq!(
        (view.state, view.problem.is_some()),
        (FolderState::UpToDate, true)
    );
}
