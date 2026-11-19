// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn a_gitignore_choice_whose_sync_fails_after_publication_is_what_a_reopen_reads() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    fs::write(&paths.config_file, "[sync]\n").unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(driver.follows_gitignore());
    let failing = koidrive_state_file::fail_syncs_after_publication();
    let set = driver.set_gitignore(false).await;
    drop(failing);
    assert!(set.is_ok());
    assert!(!driver.follows_gitignore());
    drop(driver);
    assert!(!Driver::open(paths).unwrap().follows_gitignore());
}

#[tokio::test]
async fn setting_gitignore_keeps_the_rest_of_the_configuration_file() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let text = "# mine\n[sync]\nexclude = [\"*.iso\"] # big files\n\n[upload]\nconcurrency = 3\n";
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    fs::write(&paths.config_file, text).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(driver.follows_gitignore());

    driver.set_gitignore(false).await.unwrap();
    assert!(!driver.follows_gitignore());
    let off = "# mine\n[sync]\nexclude = [\"*.iso\"] # big files\ngitignore = false\n\n[upload]\nconcurrency = 3\n";
    assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), off);
    driver.set_gitignore(false).await.unwrap();
    assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), off);
    drop(driver);
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(
        !driver.follows_gitignore(),
        "the next start applies the file"
    );

    driver.set_gitignore(true).await.unwrap();
    assert_eq!(
        fs::read_to_string(&paths.config_file).unwrap(),
        off.replace("gitignore = false", "gitignore = true")
    );
    let config = Config::load(&paths.config_file).unwrap();
    assert!(config.sync.gitignore);
    assert_eq!(config.sync.exclude, ["*.iso"]);
    assert_eq!(config.upload.concurrency.get(), 3);

    let fresh = Paths::under(&base.join("fresh"));
    let driver = Driver::open(fresh.clone()).unwrap();
    driver.set_gitignore(false).await.unwrap();
    assert_eq!(
        fs::read_to_string(&fresh.config_file).unwrap(),
        "[sync]\ngitignore = false\n"
    );
}

#[tokio::test]
async fn setting_gitignore_restarts_each_running_folder_once() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    let driver = Driver::open(paths).unwrap();
    driver.start();
    let done = |driver: &Driver| {
        let folders = driver.inner.folders.lock().unwrap();
        folders[SPACE].run.as_ref().unwrap().done.clone()
    };
    let first = done(&driver);

    driver.set_gitignore(false).await.unwrap();
    assert!(first.has_changed().is_err(), "the task that ran has ended");
    let second = done(&driver);
    assert!(second.has_changed().is_ok(), "a new task runs");

    driver.set_gitignore(false).await.unwrap();
    assert!(
        second.has_changed().is_ok(),
        "the same value restarts nothing"
    );
    driver.stop().await;
}

#[tokio::test]
async fn gitignore_only_counts_the_files_only_gitignore_files_leave_out() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    fs::write(&paths.config_file, "[sync]\nexclude = [\"vendor/\"]\n").unwrap();
    for (path, text) in [
        (".gitignore", "node_modules/\n*.log\nvendor/\n"),
        (".koiignore", "secret.log\n!keep.log\n"),
        ("node_modules/a.js", "abc"),
        ("node_modules/pkg/b.js", "12345"),
        ("node_modules/pkg/.koiignore", "c.js\n"),
        ("node_modules/pkg/c.js", "left out by .koiignore"),
        ("x.log", "12"),
        ("secret.log", "left out by .koiignore"),
        ("keep.log", "kept by .koiignore"),
        ("vendor/v.log", "left out by the device"),
        ("notes.txt", "synced"),
        ("sub/.gitignore", "*.tmp\n"),
        ("sub/t.tmp", "tmp"),
        (".DS_Store", "built in"),
        (".git/HEAD", "ref: refs/heads/main"),
        (".git/logs/x.log", "left out with every .git folder"),
        (
            "sub/.git/objects/pack/p.pack",
            "left out with every .git folder",
        ),
    ] {
        fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        fs::write(root.join(path), text).unwrap();
    }
    std::os::unix::fs::symlink(root.join("notes.txt"), root.join("link.log")).unwrap();
    let driver = Driver::open(paths).unwrap();
    let only = GitignoreOnly {
        files: 5,
        bytes: 18,
        unreadable: Vec::new(),
    };
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), only);
    driver.set_gitignore(false).await.unwrap();
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), only);
    assert!(matches!(
        driver.gitignore_only("0190f0a0-0000-7000-8000-000000000002"),
        Err(DriverError::SpaceNotLinked(_))
    ));
}

#[tokio::test]
async fn gitignore_only_counts_a_linked_root_it_cannot_open_as_unreadable() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(root.join("a.log"), "abc").unwrap();
    let driver = Driver::open(paths).unwrap();
    let blocked = GitignoreOnly {
        files: 0,
        bytes: 0,
        unreadable: vec![root.clone()],
    };

    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
    let counted = driver.gitignore_only(SPACE);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(counted.unwrap(), blocked);

    fs::rename(&root, base.join("Moved")).unwrap();
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), blocked);
}

#[tokio::test]
async fn setting_gitignore_refuses_a_configuration_file_the_next_start_would_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    for text in [
        "[sync]\nexclude = [\"!keep\"]\n",
        "[sync]\nexclude = [\"!keep\"]\ngitignore = false\n",
    ] {
        fs::write(&paths.config_file, text).unwrap();
        assert!(Config::load(&paths.config_file).is_err());
        assert!(matches!(
            driver.set_gitignore(false).await,
            Err(DriverError::Config(ConfigError::Invalid { .. }))
        ));
        assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), text);
    }
    assert!(driver.follows_gitignore());
}

fn gitignore_only_of(base: &Path, root: &Path) -> Driver {
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, root, "d1");
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(root.join("a.log"), "abc").unwrap();
    Driver::open(paths).unwrap()
}

#[tokio::test]
async fn gitignore_only_does_not_count_a_folder_made_where_the_linked_root_was() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let driver = gitignore_only_of(&base, &root);
    fs::rename(&root, base.join("Moved")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(root.join("b.log"), "12345").unwrap();
    let blocked = GitignoreOnly {
        files: 0,
        bytes: 0,
        unreadable: vec![root],
    };
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), blocked);
}

#[tokio::test]
async fn gitignore_only_does_not_count_the_folder_a_symbolic_link_puts_above_a_linked_root() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Parent/Koidrive");
    let driver = gitignore_only_of(&base, &root);
    fs::rename(base.join("Parent"), base.join("Moved")).unwrap();
    let other = base.join("Other/Koidrive");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join(".gitignore"), "*.log\n").unwrap();
    fs::write(other.join("b.log"), "12345").unwrap();
    std::os::unix::fs::symlink(base.join("Other"), base.join("Parent")).unwrap();
    let blocked = GitignoreOnly {
        files: 0,
        bytes: 0,
        unreadable: vec![root],
    };
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), blocked);
}

#[tokio::test]
async fn gitignore_only_counts_a_linked_root_a_symbolic_link_leads_to() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let driver = gitignore_only_of(&base, &root);
    let moved = base.join("Documents/Koidrive");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(&root, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &root).unwrap();
    let counted = GitignoreOnly {
        files: 1,
        bytes: 3,
        unreadable: Vec::new(),
    };
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), counted);
}

#[tokio::test]
async fn gitignore_only_counts_a_linked_root_only_when_its_marker_admits_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let driver = gitignore_only_of(&base, &root);
    let tokens = koidrive_fs::RootTokens {
        current: Some("b".repeat(32)),
        pending: None,
    };
    (Store::open(driver.paths().store(SPACE).unwrap()).unwrap())
        .transaction(|tx| tx.set_root_tokens(&tokens))
        .unwrap();
    let put_back = |effects| Marker {
        space_id: SPACE.into(),
        effects,
        token: Some("a".repeat(32)),
    };
    write_marker(&root, &put_back(0)).unwrap();
    let blocked = GitignoreOnly {
        files: 0,
        bytes: 0,
        unreadable: vec![root.clone()],
    };
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), blocked);

    write_marker(&root, &put_back(1)).unwrap();
    let counted = GitignoreOnly {
        files: 1,
        bytes: 3,
        unreadable: Vec::new(),
    };
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), counted);
    assert_eq!(koidrive_fs::read_marker(&root).unwrap(), Some(put_back(1)));
    let stored = (Store::open(driver.paths().store(SPACE).unwrap()).unwrap())
        .transaction(|tx| tx.root_tokens())
        .unwrap();
    assert_eq!(stored, tokens);
}

#[tokio::test]
async fn gitignore_only_does_not_wait_for_the_folders_write_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let driver = gitignore_only_of(&base, &root);
    let mut store = Store::open(driver.paths().store(SPACE).unwrap()).unwrap();
    let (counted, took) = store
        .transaction(|_| {
            let started = Instant::now();
            let counted = driver.gitignore_only(SPACE).unwrap();
            Ok::<_, koidrive_store::Error>((counted, started.elapsed()))
        })
        .unwrap();
    let only = GitignoreOnly {
        files: 1,
        bytes: 3,
        unreadable: Vec::new(),
    };
    assert_eq!(counted, only);
    assert!(took < Duration::from_secs(1), "took {took:?}");
}

#[tokio::test]
async fn gitignore_only_leaves_out_a_folder_while_it_is_unlinked() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let driver = gitignore_only_of(&base, &base.join("Koidrive"));
    (driver.inner.folders.lock().unwrap().get_mut(SPACE).unwrap()).unlink = Some(Unlink::Running);
    assert!(matches!(
        driver.gitignore_only(SPACE),
        Err(DriverError::SpaceNotLinked(_))
    ));
}
