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
fn a_file_another_device_makes_at_a_kept_folders_place_is_reported_and_the_folder_stays() {
    let ignored = [".env"];
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_ignored_entries(&["README.md"], &ignored);

    a.create(&mut remote, &site_a.root.join("app"), b"a file now");
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);

    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert_eq!(remote.names(), ["app"]);
    let env = site.root.join("app/.env");
    assert_eq!(fs::read_to_string(&env).unwrap(), local_only(&env));
    let view = b.folder.view();
    assert_eq!(view.state, FolderState::UpToDate);
    assert!(view.problem.is_some_and(|problem| problem.contains("\"app\"")));
}

#[test]
fn a_rule_that_lets_a_kept_entry_in_uploads_the_kept_folder_as_a_new_folder() {
    let ignored = ["build.log"];
    let (mut remote, _, _, site, b, old) = deleted_with_ignored_entries(&["README.md"], &ignored);
    drop(b);

    let mut b = site.start_with(&SyncRules {
        include: Vec::new(),
        exclude: vec![".env".into()],
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
