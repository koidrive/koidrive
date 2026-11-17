// SPDX-License-Identifier: Apache-2.0

fn not_following_gitignore() -> SyncRules {
    SyncRules {
        gitignore: false,
        ..SyncRules::default()
    }
}

fn started_with(site: &Site, remote: &mut Remote, sync: &SyncRules) -> Device {
    let mut device = site.start_with(sync);
    device.connect(remote);
    device.scan_until(|device| device.folder.scanned);
    device.rest(remote);
    device
}

fn on_server(remote: &mut Remote, name: &str) -> Vec<(String, String)> {
    let site = Site::new(remote, name);
    started_with(&site, remote, &not_following_gitignore());
    files(&site.root)
}

fn texts(files: &[(&str, &str)]) -> Vec<(String, String)> {
    (files.iter())
        .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
        .collect()
}

#[test]
fn following_gitignore_files_leaves_what_synced_on_disk_and_on_the_server() {
    let mut remote = Remote::new();
    let site = Site::new(&remote, "b");
    let mut b = started_with(&site, &mut remote, &not_following_gitignore());
    fs::create_dir(site.root.join("build")).unwrap();
    fs::write(site.root.join(".gitignore"), "build/\n*.log\n").unwrap();
    fs::write(site.root.join("build/out.bin"), "binary").unwrap();
    fs::write(site.root.join("debug.log"), "log").unwrap();
    fs::write(site.root.join("notes.txt"), "notes").unwrap();
    b.rescan();
    b.rest(&mut remote);
    let synced = texts(&[
        (".gitignore", "build/\n*.log\n"),
        ("build/out.bin", "binary"),
        ("debug.log", "log"),
        ("notes.txt", "notes"),
    ]);
    assert_eq!(files(&site.root), synced);
    let tree = remote.tree();
    assert_eq!(tree.len(), 5);
    drop(b);

    let mut b = started_with(&site, &mut remote, &SyncRules::default());
    let excluded = |b: &Device, name: &str| {
        let (id, _) = (b.folder.engine.trees.local().iter())
            .find(|(_, node)| node.shape.name == name)
            .unwrap();
        b.folder.engine.trees.excluded(id)
    };
    assert!(excluded(&b, "debug.log") && excluded(&b, "build"));
    assert!(!excluded(&b, "notes.txt"));
    assert!(
        b.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(files(&site.root), synced);
    assert!(trashed(&site.root).is_empty());
    assert_eq!(remote.tree(), tree);

    let sent = b.sent.len();
    fs::write(site.root.join("debug.log"), "log edited while left out").unwrap();
    fs::write(site.root.join("build/out.bin"), "rebuilt").unwrap();
    b.rescan();
    b.settle();
    assert!(b.work.is_empty());
    assert_eq!(
        b.sent.len(),
        sent,
        "an edit the rules leave out uploads nothing"
    );
    assert_eq!(remote.tree(), tree);
    assert_eq!(on_server(&mut remote, "c"), synced);
}

#[test]
fn not_following_gitignore_files_uploads_what_they_left_out_and_resumes_what_synced_before() {
    let mut remote = Remote::new();
    let site = Site::new(&remote, "b");
    let mut b = started_with(&site, &mut remote, &not_following_gitignore());
    fs::write(site.root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(site.root.join("debug.log"), "v1").unwrap();
    b.rescan();
    b.rest(&mut remote);
    let debug = remote.tree()["debug.log"].clone();
    drop(b);

    let mut b = started_with(&site, &mut remote, &SyncRules::default());
    fs::write(site.root.join("debug.log"), "v2 while left out").unwrap();
    fs::write(site.root.join("new.log"), "never synced").unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert_eq!(
        on_server(&mut remote, "c"),
        texts(&[(".gitignore", "*.log\n"), ("debug.log", "v1")])
    );
    drop(b);

    let b = started_with(&site, &mut remote, &not_following_gitignore());
    assert!(
        b.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(
        remote.tree()["debug.log"],
        debug,
        "the file synced before keeps its node"
    );
    let now = texts(&[
        (".gitignore", "*.log\n"),
        ("debug.log", "v2 while left out"),
        ("new.log", "never synced"),
    ]);
    assert_eq!(files(&site.root), now);
    assert_eq!(on_server(&mut remote, "d"), now);
}

#[test]
fn a_link_whose_configuration_has_no_gitignore_key_follows_gitignore_files() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    fs::write(&config, "[sync]\nexclude = [\"*.tmp\"]\n").unwrap();
    let sync = crate::Config::load(&config).unwrap().sync;
    let mut remote = Remote::new();
    let site = Site::new(&remote, "b");
    let mut b = started_with(&site, &mut remote, &sync);
    fs::create_dir(site.root.join("node_modules")).unwrap();
    fs::write(site.root.join(".gitignore"), "node_modules/\n").unwrap();
    fs::write(site.root.join("node_modules/x.js"), "x").unwrap();
    fs::write(site.root.join("cache.tmp"), "tmp").unwrap();
    fs::write(site.root.join("index.js"), "index").unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert_eq!(remote.names(), [".gitignore", "index.js"]);
}
