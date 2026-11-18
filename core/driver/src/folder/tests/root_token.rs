// SPDX-License-Identifier: Apache-2.0

fn opened(site: &Site) -> Result<Folder, Stop> {
    let mut link = site.link.clone();
    disk::admit_root(&mut link, &mut Store::open(&site.db).unwrap())?;
    let lock = claim::lock(&site.root)
        .unwrap()
        .expect("the folder is not locked");
    let local = local_rules(&SyncRules::default()).unwrap();
    Folder::open(link, &lock, &site.db, local)
}

fn halts_as_a_copy(site: &Site) -> bool {
    matches!(opened(site), Err(Stop::Root(RootProblem::Copy)))
}

fn marker_of(site: &Site) -> Marker {
    koidrive_fs::read_marker(&site.root).unwrap().unwrap()
}

fn tokens_of(site: &Site) -> koidrive_fs::RootTokens {
    Store::open(&site.db)
        .unwrap()
        .transaction(|tx| tx.root_tokens())
        .unwrap()
}

fn assert_rotated_from(site: &Site, earlier: &[Option<String>]) {
    let token = marker_of(site).token;
    assert!(
        token
            .as_ref()
            .is_some_and(|token| token.len() == 32 && token.chars().all(|c| c.is_ascii_hexdigit())),
        "{token:?}"
    );
    assert!(!earlier.contains(&token), "{token:?} in {earlier:?}");
    assert_eq!(
        tokens_of(site),
        koidrive_fs::RootTokens {
            current: token,
            pending: None,
        }
    );
}

fn copy_folder(from: &Path, to: &Path) {
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn a_copy_of_the_folder_made_before_its_last_start_halts_as_a_copy() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    drop(a);
    let copy = site.root.with_file_name("copy");
    copy_folder(&site.root, &copy);
    let mut a = restarted(&site, &mut remote);
    a.create(&mut remote, &site.root.join("after.txt"), b"after");
    drop(a);

    let original = site.root.with_file_name("original");
    fs::rename(&site.root, &original).unwrap();
    fs::rename(&copy, &site.root).unwrap();
    let copied = marker_of(&site);
    assert!(halts_as_a_copy(&site));
    assert_eq!(marker_of(&site), copied);
    assert_eq!(names(&site.root), ["before.txt"]);
    assert_eq!(remote.names(), ["after.txt", "before.txt"]);

    fs::rename(&site.root, &copy).unwrap();
    fs::rename(&original, &site.root).unwrap();
    let a = restarted(&site, &mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    assert_eq!(names(&site.root), ["after.txt", "before.txt"]);
    assert_eq!(remote.names(), ["after.txt", "before.txt"]);
}

#[test]
fn a_marker_without_a_token_takes_one_at_its_first_start_and_another_at_each_start() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("kept.txt"), b"kept");
    drop(a);
    let effects = marker_of(&site).effects;
    fs::write(
        site.root.join(koidrive_fs::PRIVATE_DIR).join("space.json"),
        format!(r#"{{"space_id":"{}","effects":{effects}}}"#, site.link.space_id),
    )
    .unwrap();
    let mut store = Store::open(&site.db).unwrap();
    store
        .transaction(|tx| tx.set_root_tokens(&koidrive_fs::RootTokens::default()))
        .unwrap();
    drop(store);

    let a = restarted(&site, &mut remote);
    assert_rotated_from(&site, &[None]);
    assert_ne!(a.state(), FolderState::Halted);
    assert_eq!(names(&site.root), ["kept.txt"]);
    assert_eq!(remote.names(), ["kept.txt"]);
    drop(a);
    let first = marker_of(&site).token;
    drop(site.start());
    assert_rotated_from(&site, &[None, first]);
}

#[test]
fn a_crash_at_each_step_of_a_rotation_starts_again_without_a_halt() {
    for upgraded in [false, true] {
        for written in [false, true] {
            let context = format!("upgraded {upgraded}, marker written {written}");
            let mut remote = Remote::new();
            let (site, a) = linked(&mut remote, "a");
            drop(a);
            let mut marker = marker_of(&site);
            if upgraded {
                marker.token = None;
                write_marker(&site.root, &marker).unwrap();
            }
            let mut store = Store::open(&site.db).unwrap();
            let pending = disk::pend_token(&marker, &mut store).unwrap();
            if written {
                let rotated = Marker {
                    token: Some(pending.clone()),
                    ..marker.clone()
                };
                write_marker(&site.root, &rotated).unwrap();
            }
            drop(store);

            let mut a = restarted(&site, &mut remote);
            assert_rotated_from(&site, &[marker.token.clone(), Some(pending)]);
            a.create(&mut remote, &site.root.join("after.txt"), b"after");
            assert_eq!(remote.names(), ["after.txt"], "{context}");
        }
    }
}

#[test]
fn a_marker_the_start_cannot_write_keeps_its_token_until_a_start_can() {
    let mut remote = Remote::new();
    let (site, a) = linked(&mut remote, "a");
    drop(a);
    let before = marker_of(&site).token;
    let blocker = site
        .root
        .join(koidrive_fs::PRIVATE_DIR)
        .join("space.json.tmp");
    fs::create_dir(&blocker).unwrap();
    fs::write(blocker.join("held"), b"").unwrap();

    let a = restarted(&site, &mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    assert_eq!(marker_of(&site).token, before);
    let tokens = tokens_of(&site);
    assert_eq!(tokens.current, before);
    drop(a);
    drop(site.start());
    assert_eq!(marker_of(&site).token, before);

    fs::remove_dir_all(&blocker).unwrap();
    drop(site.start());
    assert_rotated_from(&site, &[before, tokens.pending]);
}

#[test]
fn a_store_older_than_the_folder_recovers_whatever_token_the_folder_carries() {
    let mut remote = Remote::new();
    let (site, a) = linked(&mut remote, "a");
    drop(a);
    let saved = site.db.with_extension("saved");
    fs::copy(&site.db, &saved).unwrap();
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    let mut a = restarted(&site, &mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["two.txt"]);
    drop(a);
    fs::copy(&saved, &site.db).unwrap();
    assert_ne!(tokens_of(&site).current, marker_of(&site).token);

    let a = site.start();
    assert!(matches!(
        a.folder.engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));
    assert_rotated_from(&site, &[]);
}

#[test]
fn two_copies_of_one_disk_halt_when_the_older_one_is_at_the_folder() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let mut remote = Remote::new();
    let site = Site::new(&remote, "a");
    let image = disks.image("linked");
    disks.attach(&image, &site.root);
    let marker = Marker {
        space_id: site.link.space_id.clone(),
        effects: 0,
        token: None,
    };
    write_marker(&site.root, &marker).unwrap();
    let found = found_root(&site.root);
    let link = Link {
        root_dev: found.dev,
        root_volume: found.volume,
        ..site.link.clone()
    };
    let mut store = Store::open(&site.db).unwrap();
    store.transaction(|tx| tx.set_link(&link)).unwrap();
    drop(store);
    let mut a = restarted(&site, &mut remote);
    a.create(&mut remote, &site.root.join("before.txt"), b"before");
    drop(a);
    disks.detach(&site.root);
    let copy = disks.copy(&image, "copy");

    disks.attach(&image, &site.root);
    let mut a = restarted(&site, &mut remote);
    a.create(&mut remote, &site.root.join("after.txt"), b"after");
    drop(a);
    disks.detach(&site.root);

    disks.attach(&copy, &site.root);
    assert_eq!(found_root(&site.root).volume, found.volume);
    assert!(halts_as_a_copy(&site));
    assert!(site.root.join("before.txt").exists());
    assert!(!site.root.join("after.txt").exists());
    assert!(remote.tree().contains_key("after.txt"));
    disks.detach(&site.root);

    disks.attach(&image, &site.root);
    let a = restarted(&site, &mut remote);
    assert_ne!(a.state(), FolderState::Halted);
    assert!(remote.tree().contains_key("after.txt"));
    drop(a);
    disks.detach(&site.root);
}
