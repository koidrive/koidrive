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
fn a_folder_deleted_while_a_gitignore_file_leaves_it_out_downloads_again_when_the_device_stops_following_them()
 {
    let count = 3;
    let mut remote = Remote::new();
    let site = Site::new(&remote, "b");
    let mut b = started_with(&site, &mut remote, &not_following_gitignore());
    fs::create_dir_all(site.root.join("dist/sub")).unwrap();
    fs::write(site.root.join(".gitignore"), "dist/\n").unwrap();
    for n in 0..count {
        let dir = if n % 2 == 0 { "dist" } else { "dist/sub" };
        fs::write(
            site.root.join(format!("{dir}/f{n}.docx")),
            format!("report {n}"),
        )
        .unwrap();
    }
    b.rescan();
    b.rest(&mut remote);
    let synced = files(&site.root);
    assert_eq!(synced.len(), count + 1);
    let server = remote.tree();
    drop(b);

    let mut b = started_with(&site, &mut remote, &SyncRules::default());
    fs::remove_dir_all(site.root.join("dist")).unwrap();
    b.rescan();
    b.rest(&mut remote);
    assert!(b.sent.is_empty(), "nothing is sent while dist is left out");
    drop(b);

    let b = started_with(&site, &mut remote, &not_following_gitignore());
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert!(!sent.contains(&"delete"), "sent {sent:?}");
    assert_eq!(remote.tree(), server);
    assert_eq!(files(&site.root), synced);
    assert!(trashed(&site.root).is_empty());
    assert!(shown(&site).is_empty(), "no question opens");
    assert_eq!(b.state(), FolderState::UpToDate);
}

fn repository(root: &Path, at: &str) {
    for (path, text) in [
        (".git/HEAD", "ref: refs/heads/main\n"),
        (".git/config", "[core]\n\tfsmonitor = .git/hooks/watch\n"),
        (".git/hooks/pre-commit", "#!/bin/sh\n"),
        ("README.md", "readme"),
    ] {
        let path = root.join(at).join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}

fn deletes(device: &Device) -> Vec<String> {
    (device.sent.iter().flat_map(|commit| &commit.ops))
        .filter_map(|op| match &op.op {
            Some(op::Op::Delete(delete)) => Some(delete.node_id.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_repository_s_git_folder_never_uploads_whether_the_device_follows_gitignore_files_or_not() {
    for sync in [SyncRules::default(), not_following_gitignore()] {
        let mut remote = Remote::new();
        let site = Site::new(&remote, "b");
        let mut b = started_with(&site, &mut remote, &sync);
        repository(&site.root, "Code/app");
        repository(&site.root, "Code/app/vendor/lib");
        fs::create_dir(site.root.join("Code/app/sub")).unwrap();
        fs::write(
            site.root.join("Code/app/sub/.git"),
            "gitdir: ../.git/modules/sub\n",
        )
        .unwrap();
        b.rescan();
        b.rest(&mut remote);
        assert_eq!(
            remote.tree().keys().collect::<Vec<_>>(),
            [
                "Code",
                "Code/app",
                "Code/app/README.md",
                "Code/app/sub",
                "Code/app/sub/.git",
                "Code/app/vendor",
                "Code/app/vendor/lib",
                "Code/app/vendor/lib/README.md",
            ],
            "gitignore {}",
            sync.gitignore
        );
        assert_eq!(
            on_server(&mut remote, "c"),
            texts(&[
                ("Code/app/README.md", "readme"),
                ("Code/app/sub/.git", "gitdir: ../.git/modules/sub\n"),
                ("Code/app/vendor/lib/README.md", "readme"),
            ])
        );
    }
}

#[test]
fn a_koiignore_pattern_written_while_the_device_is_stopped_uploads_git_folders_through_a_server_error()
 {
    let mut remote = Remote::new();
    let site = Site::new(&remote, "b");
    let mut b = started_with(&site, &mut remote, &SyncRules::default());
    repository(&site.root, "app");
    b.rescan();
    b.rest(&mut remote);
    let site_c = Site::new(&remote, "c");
    let mut c = started_with(&site_c, &mut remote, &SyncRules::default());
    let without_git = SyncRules {
        exclude: vec![".git/".into()],
        ..SyncRules::default()
    };
    let site_d = Site::new(&remote, "d");
    let mut d = started_with(&site_d, &mut remote, &without_git);
    assert_eq!(
        remote.tree().keys().collect::<Vec<_>>(),
        ["app", "app/README.md"]
    );
    drop(b);

    fs::write(site.root.join("app/.koiignore"), "!.git/\n").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|device| device.folder.scanned);
    let mut rounds = 0;
    let refused = loop {
        rounds += 1;
        assert!(rounds < 1000, "the folder sends a commit");
        match b.work.pop_front() {
            Some(Work::Send(commit)) => break commit,
            Some(work) => b.perform(work, &mut remote).unwrap(),
            None => b.settle(),
        }
    };
    b.sent.push(refused);
    let unavailable = put_off(koidrive_proto::v1::error::Code::RetryLater);
    let step = b.folder.sent(Err(CallError::Server(unavailable)));
    b.take(step).unwrap();
    b.rest(&mut remote);
    assert!(deletes(&b).is_empty());
    let everything = files(&site.root);
    assert_eq!(
        everything
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        [
            "app/.git/HEAD",
            "app/.git/config",
            "app/.git/hooks/pre-commit",
            "app/.koiignore",
            "app/README.md",
        ]
    );
    assert_eq!(on_server(&mut remote, "e"), everything);

    c.pull(&remote);
    c.rest(&mut remote);
    assert_eq!(files(&site_c.root), everything);
    assert!(c.sent.is_empty());

    d.pull(&remote);
    d.rest(&mut remote);
    assert_eq!(
        files(&site_d.root),
        texts(&[("app/.koiignore", "!.git/\n"), ("app/README.md", "readme")])
    );
    assert!(d.sent.is_empty());
}

#[test]
fn renaming_and_moving_a_folder_that_holds_a_repository_moves_it_without_deleting_anything() {
    for included in [false, true] {
        let mut remote = Remote::new();
        let site = Site::new(&remote, "b");
        let mut b = started_with(&site, &mut remote, &SyncRules::default());
        repository(&site.root, "Projects/app");
        fs::create_dir(site.root.join("Archive")).unwrap();
        if included {
            fs::write(site.root.join(".koiignore"), "!.git/\n").unwrap();
        }
        b.rescan();
        b.rest(&mut remote);
        let before = remote.tree();
        let site_c = Site::new(&remote, "c");
        let mut c = started_with(&site_c, &mut remote, &SyncRules::default());

        fs::rename(site.root.join("Projects"), site.root.join("Work")).unwrap();
        b.rescan();
        b.rest(&mut remote);
        fs::rename(site.root.join("Work/app"), site.root.join("Archive/app")).unwrap();
        b.rescan();
        b.rest(&mut remote);
        assert!(deletes(&b).is_empty(), "included {included}");

        let moved = |path: &str| {
            path.replacen("Projects/app", "Archive/app", 1)
                .replacen("Projects", "Work", 1)
        };
        let expected: BTreeMap<String, String> = (before.iter())
            .map(|(path, id)| (moved(path), id.clone()))
            .collect();
        assert_eq!(remote.tree(), expected, "included {included}");
        assert_eq!(
            expected.contains_key("Archive/app/.git/HEAD"),
            included,
            "included {included}"
        );
        let git = |root: &Path| fs::read_to_string(root.join("Archive/app/.git/HEAD")).ok();
        assert_eq!(git(&site.root).as_deref(), Some("ref: refs/heads/main\n"));

        c.pull(&remote);
        c.rest(&mut remote);
        assert!(deletes(&c).is_empty());
        assert_eq!(files(&site_c.root), on_server(&mut remote, "d"));
        assert_eq!(git(&site_c.root).is_some(), included);
    }
}

#[test]
fn a_synced_file_moved_into_a_left_out_git_folder_stays_on_the_server() {
    let mut remote = Remote::new();
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir_all(site.root.join("repo/.git")).unwrap();
    fs::write(site.root.join("repo/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    b.create(&mut remote, &site.root.join("repo/notes.txt"), b"kept");
    let before = remote.tree();
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        ["repo", "repo/notes.txt"]
    );

    fs::rename(
        site.root.join("repo/notes.txt"),
        site.root.join("repo/.git/notes.txt"),
    )
    .unwrap();
    b.rescan();
    b.settle();
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert!(deletes(&b).is_empty(), "a move into .git deletes nothing");
    assert_eq!(remote.tree(), before);
    assert_eq!(
        fs::read(site.root.join("repo/.git/notes.txt")).unwrap(),
        b"kept"
    );
    assert!(
        b.folder
            .engine
            .trees
            .excluded(&NodeId::from(&before["repo/notes.txt"]))
    );
}
