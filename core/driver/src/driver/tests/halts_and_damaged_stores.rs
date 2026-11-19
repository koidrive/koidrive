// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn pausing_a_halted_folder_keeps_its_problem_and_resuming_rechecks_it() {
    let (_dir, paths, _root) = profile(&[]);
    credential("d2").save_new(&paths.credential()).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    let halted = driver.status().folders[0].clone();
    assert_eq!(halted.state, FolderState::Halted);
    assert!(halted.problem.as_ref().unwrap().contains("device d1"));
    driver.pause().await.unwrap();
    let paused = driver.status();
    assert!(paused.paused);
    assert_eq!(paused.folders[0].state, FolderState::Halted);
    assert_eq!(paused.folders[0].problem, halted.problem);
    driver.resume().await.unwrap();
    assert_eq!(driver.status().folders[0].state, FolderState::Halted);
    assert_eq!(driver.status().folders[0].problem, halted.problem);
    driver.pause().await.unwrap();
    driver.stop().await;
    drop(driver);

    let driver = Driver::open(paths).unwrap();
    driver.start();
    assert!(driver.status().paused);
    assert_eq!(driver.status().folders[0].state, FolderState::Halted);
    assert_eq!(driver.status().folders[0].problem, halted.problem);
    driver.resume().await.unwrap();
    assert_eq!(driver.status().folders[0].state, FolderState::Halted);
    assert_eq!(driver.status().folders[0].problem, halted.problem);
    driver.stop().await;
}

#[tokio::test]
async fn a_resume_and_a_reopen_keep_a_replacement_folder_waiting_for_explicit_restart() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        eventually("the original folder is claimed", || {
            claim::owner(&root).unwrap().is_some()
        })
        .await;
        fs::rename(&root, base.join("Moved")).unwrap();
        fs::create_dir(&root).unwrap();
        write_marker(
            &root,
            &Marker {
                space_id: SPACE.into(),
                effects: 0,
                token: None,
            },
        )
        .unwrap();
        eventually("the replacement requires a restart", || {
            driver.status().folders[0]
                .problem
                .as_ref()
                .is_some_and(|problem| problem.contains("koi folder restart"))
        })
        .await;
        let problem = driver.status().folders[0].problem.clone();
        driver.pause().await.unwrap();
        driver.resume().await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(claim::owner(&root).unwrap(), None);
        assert_eq!(driver.status().folders[0].state, FolderState::Halted);
        assert_eq!(driver.status().folders[0].problem, problem);
        driver.pause().await.unwrap();
        driver.stop().await;
        drop(driver);

        let driver = Driver::open(paths).unwrap();
        driver.start();
        assert!(driver.status().paused);
        assert_eq!(driver.status().folders[0].state, FolderState::Halted);
        assert_eq!(driver.status().folders[0].problem, problem);
        driver.resume().await.unwrap();
        assert_eq!(claim::owner(&root).unwrap(), None);
        assert_eq!(driver.status().folders[0].state, FolderState::Halted);
        driver.restart(&root).await.unwrap();
        eventually("the explicit restart claims the replacement", || {
            claim::owner(&root).unwrap().is_some()
        })
        .await;
        driver.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_paused_root_wait_reopens_and_recovers_only_the_original_folder() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let moved = base.join("Moved");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        eventually("the folder is claimed", || {
            claim::owner(&root).unwrap().is_some()
        })
        .await;
        fs::rename(&root, &moved).unwrap();
        halted(&driver).await;
        driver.pause().await.unwrap();
        let problem = driver.status().folders[0].problem.clone();
        driver.stop().await;
        drop(driver);

        let driver = Driver::open(paths).unwrap();
        driver.start();
        assert_eq!(driver.status().folders[0].state, FolderState::Halted);
        assert_eq!(driver.status().folders[0].problem, problem);
        fs::rename(&moved, &root).unwrap();
        driver.resume().await.unwrap();
        eventually("the original folder syncs again", || {
            driver.status().folders[0].state != FolderState::Halted
                && claim::lock(&root).unwrap().is_none()
        })
        .await;
        driver.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_failed_open_preserves_the_claimed_root_after_reopen() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        let original = claim::Identity::of(&fs::metadata(&root).unwrap());
        crate::folder::BEFORE_OPEN.with(|hook| {
            *hook.borrow_mut() = Some(Box::new({
                let root = root.clone();
                move || {
                    write_marker(
                        &root,
                        &Marker {
                            space_id: "another-space".into(),
                            effects: 0,
                            token: None,
                        },
                    )
                    .unwrap();
                }
            }));
        });
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        halted(&driver).await;
        assert!(claim::owner(&root).unwrap().is_some());
        assert_eq!(
            read_marker(&root).unwrap().unwrap().space_id,
            "another-space"
        );
        assert!(
            !driver
                .inner
                .folders
                .lock()
                .unwrap()
                .get(SPACE)
                .unwrap()
                .status
                .needs_restart()
        );
        driver.stop().await;
        drop(driver);
        fs::rename(&root, base.join("Moved")).unwrap();
        fs::create_dir(&root).unwrap();
        write_marker(
            &root,
            &Marker {
                space_id: SPACE.into(),
                effects: 0,
                token: None,
            },
        )
        .unwrap();
        let driver = Driver::open(paths).unwrap();
        let lifecycle = driver
            .inner
            .folders
            .lock()
            .unwrap()
            .get(SPACE)
            .unwrap()
            .status
            .clone();
        let saved = lifecycle.root();
        driver.start();
        eventually(
            "the reopened folder either rejects or observes the replacement",
            || lifecycle.needs_restart() || driver.status().folders[0].local_entries.is_some(),
        )
        .await;
        let owner = claim::owner(&root).unwrap();
        let status = driver.status().folders[0].clone();
        driver.stop().await;
        assert_eq!(
            owner, None,
            "reopen claimed a replacement after Folder::open failed"
        );
        assert_eq!(saved, Some(original));
        assert_eq!(status.state, FolderState::Halted);
    })
    .await
    .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_ejected_disk_whose_mount_point_stays_resumes_under_its_old_or_a_new_device_number() {
    let mut disks = koidrive_test_support::Disks::new().unwrap();
    let base = disks.base();
    let root = base.join("Koidrive");
    let elsewhere = base.join("Elsewhere");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&elsewhere).unwrap();
    let disk = disks.image("disk");
    let other = disks.image("other");
    disks.attach(&disk, &root);
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let store = paths.store(SPACE).unwrap();
    let driver = Driver::open(paths).unwrap();
    driver.start();
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;
    let lifecycle = (driver.inner.folders.lock().unwrap())
        .get(SPACE)
        .unwrap()
        .status
        .clone();
    let waits_for_its_disk = || {
        let folder = driver.status().folders[0].clone();
        folder.state == FolderState::Halted
            && folder
                .problem
                .is_some_and(|problem| problem.contains("another disk"))
    };
    let syncs = || driver.status().folders[0].state != FolderState::Halted;

    disks.detach(&root);
    eventually("the folder waits for its disk", waits_for_its_disk).await;
    assert!(!lifecycle.needs_restart());
    disks.attach(&disk, &root);
    eventually("the folder syncs again", syncs).await;
    assert!(!lifecycle.needs_restart());

    disks.detach(&root);
    eventually("the folder waits for its disk again", waits_for_its_disk).await;
    disks.attach(&other, &elsewhere);
    disks.attach(&disk, &root);
    let dev = fs::metadata(&root).unwrap().dev();
    eventually("the folder syncs under the device number it has now", syncs).await;
    assert!(!lifecycle.needs_restart());
    let link = (Store::open(&store).unwrap().transaction(|tx| tx.link()))
        .unwrap()
        .unwrap();
    assert_eq!(link.root_dev, dev);
    driver.stop().await;
}

#[tokio::test]
async fn a_pending_start_cannot_skip_a_restarts_root_reset() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        eventually("the original folder is claimed", || {
            claim::owner(&root).unwrap().is_some()
        })
        .await;
        fs::rename(&root, base.join("Moved")).unwrap();
        halted(&driver).await;
        driver.stop().await;
        let lifecycle = driver
            .inner
            .folders
            .lock()
            .unwrap()
            .get(SPACE)
            .unwrap()
            .status
            .clone();
        assert!(!lifecycle.needs_restart());
        fs::create_dir(&root).unwrap();
        write_marker(
            &root,
            &Marker {
                space_id: SPACE.into(),
                effects: 0,
                token: None,
            },
        )
        .unwrap();
        let replacement = claim::Identity::of(&fs::metadata(&root).unwrap());
        assert_ne!(lifecycle.root(), Some(replacement));
        let (reached, stopped) = tokio::sync::oneshot::channel();
        let (proceed, released) = tokio::sync::oneshot::channel();
        *driver.inner.restart_stopped.lock().unwrap() = Some(RestartStopped {
            reached,
            proceed: released,
        });
        let restarting = tokio::spawn({
            let driver = driver.clone();
            let root = root.clone();
            async move { driver.restart(&root).await }
        });
        stopped.await.unwrap();
        driver.start();
        assert!(driver.inner.startup_pending.load(Ordering::SeqCst));
        proceed.send(()).unwrap();
        restarting.await.unwrap().unwrap();
        eventually(
            "the restart either claims or rejects the replacement",
            || lifecycle.needs_restart() || claim::owner(&root).unwrap().is_some(),
        )
        .await;
        let owner = claim::owner(&root).unwrap();
        driver.stop().await;
        assert!(
            owner.is_some(),
            "a pending start prevented the explicit root reset"
        );
        assert_eq!(lifecycle.root(), Some(replacement));
        let blocked = paths
            .store(SPACE)
            .unwrap()
            .with_file_name("lifecycle.json.tmp");
        fs::create_dir(&blocked).unwrap();
        assert!(matches!(
            driver.restart(&root).await,
            Err(DriverError::Io { .. })
        ));
        assert!(!driver.inner.startup_blocked.load(Ordering::SeqCst));
        assert_eq!(lifecycle.root(), Some(replacement));
        fs::remove_dir(blocked).unwrap();
        driver.restart(&root).await.unwrap();
        assert_eq!(lifecycle.root(), None);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_corrupt_lifecycle_record_cannot_reopen_as_a_healthy_folder() {
    let (_dir, paths, _root) = profile(&[]);
    credential("d2").save_new(&paths.credential()).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    driver.pause().await.unwrap();
    drop(driver);
    let record = paths.store(SPACE).unwrap().with_file_name("lifecycle.json");
    fs::write(record, b"not json").unwrap();
    let listed = Driver::linked_folders(&paths).unwrap();
    assert_eq!(listed[0].state, FolderState::Halted);
    let driver = Driver::open(paths).unwrap();
    driver.start();
    assert_eq!(driver.status().folders[0].state, FolderState::Halted);
    driver.stop().await;
}

#[tokio::test]
async fn an_unlink_whose_record_became_unreadable_waits_to_be_finished_not_restarted() {
    for lost in [false, true] {
        an_unlink_whose_record_is_damaged_waits(lost).await;
    }
}

async fn an_unlink_whose_record_is_damaged_waits(lost: bool) {
    let (_dir, paths, root) = profile(&[]);
    let record = paths.store(SPACE).unwrap().with_file_name("lifecycle.json");
    fs::write(record.with_file_name("unlinking"), b"").unwrap();
    match lost {
        true => drop(fs::remove_file(&record)),
        false => fs::write(&record, b"not json").unwrap(),
    }
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    let status = driver.status().folders[0].clone();
    assert_eq!(status.state, FolderState::Halted);
    assert_eq!(
        status.problem.as_deref(),
        Some(crate::lifecycle::UNFINISHED_UNLINK)
    );
    let restarted = driver.restart(&root).await;
    assert!(
        restarted.is_err() || restarted.unwrap().state == FolderState::Halted,
        "a restart lifted an unfinished unlink"
    );
    assert!(!matches!(claim::owner(&root), Ok(Some(_))));
    driver.unlink(&root).await.unwrap();
    assert!(driver.status().folders.is_empty());
    assert!(!record.parent().unwrap().exists());
    driver.stop().await;
}

#[tokio::test]
async fn a_version_that_cannot_move_leaves_the_others_brought_back_and_named() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block moves");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    let private = root.join(".koidrive");
    fs::create_dir_all(private.join("parked")).unwrap();
    fs::create_dir_all(private.join("quarantine")).unwrap();
    fs::write(private.join("parked/n1"), b"parked").unwrap();
    fs::write(private.join("quarantine/e7"), b"quarantined").unwrap();
    fs::set_permissions(private.join("parked"), fs::Permissions::from_mode(0o555)).unwrap();
    let driver = Driver::open(paths).unwrap();

    let unlinked = driver.unlink(&root).await;
    fs::set_permissions(private.join("parked"), fs::Permissions::from_mode(0o755)).unwrap();

    let (_, set_aside) = unlinked.unwrap();
    assert_eq!(set_aside.kept, [private.join("parked/n1")]);
    let [back] = &set_aside.brought_back[..] else {
        panic!("one version comes back: {set_aside:?}");
    };
    assert_eq!(back.parent(), Some(root.as_path()));
    assert_eq!(fs::read(back).unwrap(), b"quarantined");
}

#[tokio::test]
async fn a_failed_unlink_names_the_versions_it_brought_back_and_so_does_its_retry_in_a_new_process()
{
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block moves");
        return;
    }
    for reopen in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Koidrive");
        linked(&paths, &root, "d1");
        fs::create_dir_all(root.join(".koidrive/parked")).unwrap();
        fs::write(root.join(".koidrive/parked/n1"), b"parked").unwrap();
        let spaces = paths.spaces();
        fs::set_permissions(&spaces, fs::Permissions::from_mode(0o555)).unwrap();
        let driver = Driver::open(paths.clone()).unwrap();

        let failed = driver.unlink(&root).await.err();
        fs::set_permissions(&spaces, fs::Permissions::from_mode(0o755)).unwrap();
        let driver = match reopen {
            true => {
                drop(driver);
                Driver::open(paths.clone()).unwrap()
            }
            false => driver,
        };
        let (_, set_aside) = driver.unlink(&root).await.unwrap();

        let [back] = &set_aside.brought_back[..] else {
            panic!("the finished unlink names the version: {set_aside:?}");
        };
        assert_eq!(fs::read(back).unwrap(), b"parked");
        assert!(
            matches!(&failed, Some(DriverError::UnlinkFailedAfterBringingBack { brought_back, .. }) if *brought_back == set_aside.brought_back),
            "{failed:?}"
        );
        assert_eq!(fs::read_dir(&spaces).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn a_store_left_partly_removed_is_gone_from_the_folders_and_finished_by_a_later_open() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block removal");
        return;
    }
    for sign_out in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Koidrive");
        for _ in 0..2 {
            if !paths.credential().exists() {
                paired(&paths, "d1");
            }
            linked_to(&paths, &root, "d1", SPACE);
            let held = paths.store(SPACE).unwrap().with_file_name("held");
            fs::create_dir(&held).unwrap();
            fs::write(held.join("f"), b"x").unwrap();
            fs::set_permissions(&held, fs::Permissions::from_mode(0o555)).unwrap();
            let driver = Driver::open(paths.clone()).unwrap();
            assert_eq!(driver.status().folders.len(), 1);

            match sign_out {
                true => assert_eq!(driver.sign_out(true).await.unwrap().not_removed, []),
                false => drop(driver.unlink(&root).await.unwrap()),
            }
            assert!(driver.status().folders.is_empty());
        }
        let reopened = Driver::open(paths.clone()).unwrap();
        assert!(reopened.status().folders.is_empty());
        assert_eq!(fs::read_dir(paths.spaces()).unwrap().count(), 2);
        drop(reopened);
        for left in fs::read_dir(paths.spaces()).unwrap() {
            let held = left.unwrap().path().join("held");
            fs::set_permissions(held, fs::Permissions::from_mode(0o755)).unwrap();
        }
        drop(Driver::open(paths.clone()).unwrap());

        assert_eq!(fs::read_dir(paths.spaces()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn an_unlink_names_a_folder_of_set_aside_versions_it_cannot_read_as_kept() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block reads");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    let parked = root.join(".koidrive/parked");
    fs::create_dir_all(&parked).unwrap();
    fs::write(parked.join("n1"), b"parked").unwrap();
    fs::set_permissions(&parked, fs::Permissions::from_mode(0o000)).unwrap();
    let driver = Driver::open(paths).unwrap();

    let unlinked = driver.unlink(&root).await;
    fs::set_permissions(&parked, fs::Permissions::from_mode(0o755)).unwrap();

    let (_, set_aside) = unlinked.unwrap();
    assert_eq!(set_aside.kept, std::slice::from_ref(&parked));
    assert_eq!(fs::read(parked.join("n1")).unwrap(), b"parked");
}

#[tokio::test]
async fn an_unlink_reports_nothing_of_versions_another_device_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    let other = Owner {
        server: "https://other.example.com/".into(),
        device_id: "d9".into(),
    };
    claim::set_owner(&root, &other).unwrap();
    fs::create_dir_all(root.join(".koidrive/parked")).unwrap();
    fs::write(root.join(".koidrive/parked/n1"), b"theirs").unwrap();
    let driver = Driver::open(paths).unwrap();

    let (_, set_aside) = driver.unlink(&root).await.unwrap();

    assert_eq!(set_aside, SetAside::default());
    assert_eq!(
        fs::read(root.join(".koidrive/parked/n1")).unwrap(),
        b"theirs"
    );
}

#[tokio::test]
async fn an_unlink_reports_nothing_of_versions_another_device_set_aside_while_it_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    let other = Owner {
        server: "https://other.example.com/".into(),
        device_id: "d9".into(),
    };
    claim::set_owner(&root, &other).unwrap();
    fs::create_dir_all(root.join(".koidrive/parked")).unwrap();
    fs::write(root.join(".koidrive/parked/n1"), b"theirs").unwrap();
    let _syncing = claim::lock(&root).unwrap().unwrap();
    let driver = Driver::open(paths).unwrap();

    let (_, set_aside) = driver.unlink(&root).await.unwrap();

    assert_eq!(set_aside, SetAside::default());
}

#[tokio::test]
async fn an_unlink_names_a_symlinked_folder_of_set_aside_versions_as_kept_and_reads_nothing_through_it()
 {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    let elsewhere = base.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("mine"), b"mine").unwrap();
    let parked = root.join(".koidrive/parked");
    fs::create_dir_all(root.join(".koidrive")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &parked).unwrap();
    let driver = Driver::open(paths).unwrap();

    let (_, set_aside) = driver.unlink(&root).await.unwrap();

    assert_eq!(set_aside.kept, std::slice::from_ref(&parked));
    assert_eq!(fs::read(elsewhere.join("mine")).unwrap(), b"mine");
}

#[tokio::test]
async fn an_unlink_finishes_without_touching_a_koidrive_folder_that_is_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    let elsewhere = base.join("elsewhere");
    fs::rename(root.join(".koidrive"), &elsewhere).unwrap();
    fs::write(elsewhere.join("mine"), b"mine").unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join(".koidrive")).unwrap();
    let driver = Driver::open(paths).unwrap();

    driver.unlink(&root).await.unwrap();

    assert_eq!(fs::read_link(root.join(".koidrive")).unwrap(), elsewhere);
    assert_eq!(fs::read(elsewhere.join("mine")).unwrap(), b"mine");
}

#[test]
fn nothing_is_listed_through_a_symlinked_koidrive_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Koidrive");
    let elsewhere = dir.path().join("elsewhere");
    fs::create_dir_all(elsewhere.join("parked")).unwrap();
    fs::write(elsewhere.join("parked/mine"), b"mine").unwrap();
    fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join(".koidrive")).unwrap();

    assert_eq!(held_aside(&root), Vec::<PathBuf>::new());
}

fn parked_report(paths: &Paths, root: &Path) {
    let parked = root.join(".koidrive/parked/n1");
    fs::create_dir_all(parked.parent().unwrap()).unwrap();
    fs::write(&parked, b"parked").unwrap();
    let identity = fs::metadata(&parked).unwrap();
    Store::open(paths.store(SPACE).unwrap())
        .unwrap()
        .transaction(|tx| {
            tx.put_local_node(&koidrive_store::LocalNode {
                node_id: "n1".into(),
                shape: koidrive_store::Shape {
                    parent_id: SPACE.into(),
                    name: "report.docx".into(),
                    kind: koidrive_proto::v1::NodeKind::File as i32,
                    content: None,
                },
                identity: koidrive_fs::FileId::stored(u128::from(identity.ino()), None, None),
                parked: true,
                left_out_when_placed: false,
                content_from_server: false,
            })?;
            tx.set_engine_state(&EngineState {
                trees: koidrive_engine::state::TreesState {
                    dirty: ["n1".into()].into(),
                    ..Default::default()
                },
                ..EngineState::default()
            })
        })
        .unwrap();
}

#[tokio::test]
async fn an_unlink_brings_displaced_versions_back_under_recovered_names() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    parked_report(&paths, &root);
    let private = root.join(".koidrive");
    fs::create_dir_all(private.join("quarantine/e7")).unwrap();
    fs::write(private.join("quarantine/e7/notes.txt"), b"quarantined").unwrap();
    let date = koidrive_fs::utc_date(SystemTime::now());
    let taken = root.join(format!("report (recovered {date}).docx"));
    fs::write(&taken, b"the user's").unwrap();

    let driver = Driver::open(paths).unwrap();
    let (_, set_aside) = driver.unlink(&root).await.unwrap();
    if koidrive_fs::utc_date(SystemTime::now()) != date {
        eprintln!("SKIPPED: the UTC date changed during the test");
        return;
    }

    let parked = root.join(format!("report (recovered {date} 2).docx"));
    let quarantined = root.join(format!("e7 (recovered {date})"));
    assert_eq!(
        set_aside,
        SetAside {
            brought_back: vec![parked.clone(), quarantined.clone()],
            kept: Vec::new(),
        }
    );
    assert_eq!(fs::read(&taken).unwrap(), b"the user's");
    assert_eq!(fs::read(parked).unwrap(), b"parked");
    assert_eq!(
        fs::read(quarantined.join("notes.txt")).unwrap(),
        b"quarantined"
    );
}

#[tokio::test]
async fn a_failed_unlink_leaves_no_marker_to_block_a_later_restart() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    let record = paths.store(SPACE).unwrap().with_file_name("lifecycle.json");
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;
    let blocked = record.with_extension("json.tmp");
    fs::create_dir(&blocked).unwrap();
    assert!(driver.unlink(&root).await.is_err());
    fs::remove_dir(&blocked).unwrap();
    driver.stop().await;
    drop(driver);
    assert!(!record.with_file_name("unlinking").exists());

    fs::write(&record, br#"{"halt":"#).unwrap();
    let driver = Driver::open(paths).unwrap();
    driver.start();
    assert_ne!(
        driver.status().folders[0].problem.as_deref(),
        Some(crate::lifecycle::UNFINISHED_UNLINK)
    );
    let restarted = driver.restart(&root).await.unwrap();
    assert_ne!(restarted.state, FolderState::Halted);
    eventually("the restarted folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;
    driver.stop().await;
}

#[tokio::test]
async fn a_failed_retry_of_an_unfinished_unlink_keeps_it_unfinished() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    let record = paths.store(SPACE).unwrap().with_file_name("lifecycle.json");
    drop(Driver::open(paths.clone()).unwrap());
    fs::write(record.with_file_name("unlinking"), b"").unwrap();
    fs::write(&record, br#"{"halt":"#).unwrap();
    let blocked = record.with_extension("json.tmp");
    fs::create_dir(&blocked).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    let problem = |driver: &Driver| driver.status().folders[0].problem.clone();
    assert_eq!(
        problem(&driver).as_deref(),
        Some(crate::lifecycle::UNFINISHED_UNLINK)
    );
    assert!(driver.unlink(&root).await.is_err());
    assert!(record.with_file_name("unlinking").exists());
    drop(driver);

    fs::remove_dir(&blocked).unwrap();
    let driver = Driver::open(paths).unwrap();
    driver.start();
    assert_eq!(
        problem(&driver).as_deref(),
        Some(crate::lifecycle::UNFINISHED_UNLINK)
    );
    let restarted = driver.restart(&root).await;
    assert!(
        restarted.is_err() || restarted.unwrap().state == FolderState::Halted,
        "a restart lifted an unfinished unlink"
    );
    driver.unlink(&root).await.unwrap();
    assert!(!record.parent().unwrap().exists());
    driver.stop().await;
}

#[tokio::test]
async fn an_unreadable_lifecycle_record_halts_only_its_folder_until_a_restart() {
    use crate::health::tests::{named, posthog, with_prefix};

    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let (broken, healthy) = (base.join("Broken"), base.join("Healthy"));
    paired(&paths, "d1");
    linked_to(
        &paths,
        &broken,
        "d1",
        "0190f0a0-0000-7000-8000-0000000000b1",
    );
    linked_to(
        &paths,
        &healthy,
        "d1",
        "0190f0a0-0000-7000-8000-0000000000b2",
    );
    let record = paths
        .store("0190f0a0-0000-7000-8000-0000000000b1")
        .unwrap()
        .with_file_name("lifecycle.json");
    fs::write(&record, br#"{"halt":null,"ro"#).unwrap();
    let halted = Failure::Halted(Halted::Store);
    let counted = || {
        (failures::take_counted_here().get(&halted))
            .copied()
            .unwrap_or(0)
    };
    counted();

    let listed = Driver::linked_folders(&paths).unwrap();
    let state = |folders: &[FolderStatus], space: &str| {
        (folders
            .iter()
            .find(|folder| folder.space_id == space)
            .unwrap())
        .clone()
    };
    assert_eq!(
        state(&listed, "0190f0a0-0000-7000-8000-0000000000b1").state,
        FolderState::Halted
    );
    assert_eq!(counted(), 0, "a listing counts no halt");
    drop(Driver::open(paths.clone()).unwrap());
    assert_eq!(counted(), 1);
    let recorder = koidrive_telemetry::Recorder::on();
    let driver = Driver::open_with_telemetry(paths, recorder.clone()).unwrap();
    assert_eq!(counted(), 0, "a halt is counted once");
    driver.start();
    let problem = state(
        &driver.status().folders,
        "0190f0a0-0000-7000-8000-0000000000b1",
    )
    .problem
    .unwrap();
    assert!(
        problem.contains("lifecycle.json") && problem.contains("koi folder restart"),
        "{problem}"
    );
    eventually("the healthy folder is claimed", || {
        claim::owner(&healthy).unwrap().is_some()
    })
    .await;
    assert_ne!(
        state(
            &driver.status().folders,
            "0190f0a0-0000-7000-8000-0000000000b2"
        )
        .state,
        FolderState::Halted
    );
    assert!(claim::owner(&broken).unwrap().is_none());

    driver.record_health();
    let events = posthog(recorder.take());
    let report = named(&events, "hourly_report")[0];
    assert_eq!(report["l.sync.halted"], 1);
    assert_eq!(with_prefix(report, "l.halted."), [("l.halted.store", 1)]);

    let restarted = driver.restart(&broken).await.unwrap();
    assert_ne!(restarted.state, FolderState::Halted);
    eventually("the restarted folder is claimed", || {
        claim::owner(&broken).unwrap().is_some()
    })
    .await;
    let cleared = named(&posthog(recorder.take()), "problem_cleared")
        .into_iter()
        .map(|event| event["problem"].clone())
        .collect::<Vec<_>>();
    assert_eq!(cleared, ["halted.store"]);
    driver.stop().await;
    serde_json::from_slice::<serde_json::Value>(&fs::read(&record).unwrap()).unwrap();
}

#[tokio::test]
async fn a_damaged_store_whose_unlink_did_not_finish_leaves_the_other_folders_questions() {
    const UNLINKING: &str = "0190f0a0-0000-7000-8000-0000000000e1";
    const HEALTHY: &str = "0190f0a0-0000-7000-8000-0000000000e2";
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    paired(&paths, "d1");
    for space_id in [UNLINKING, HEALTHY] {
        linked_to(&paths, &base.join(space_id), "d1", space_id);
    }
    drop(Driver::open(paths.clone()).unwrap());
    let unlinking = paths.store(UNLINKING).unwrap();
    fs::write(unlinking.with_file_name("unlinking"), b"").unwrap();
    let halt = serde_json::json!({
        "halt": {
            "problem": crate::lifecycle::UNFINISHED_UNLINK,
            "recovery": "Unlink",
            "why": "unlink",
            "since": 1,
        },
    });
    fs::write(unlinking.with_file_name("lifecycle.json"), halt.to_string()).unwrap();
    garble(&paths, UNLINKING);
    Store::open(paths.store(HEALTHY).unwrap())
        .unwrap()
        .transaction(|tx| {
            tx.put_question(&StoredQuestion {
                question_id: "q1".into(),
                asked_at_ms: 10,
                body: serde_json::to_vec(&mass_delete("q1")).unwrap(),
                answer: None,
            })
        })
        .unwrap();

    let driver = Driver::open(paths.clone()).unwrap();
    let folders = driver.status().folders;
    let unlinking = (folders.iter())
        .find(|folder| folder.space_id == UNLINKING)
        .unwrap();
    assert_eq!(
        unlinking.problem.as_deref(),
        Some(crate::lifecycle::UNFINISHED_UNLINK)
    );
    assert_eq!(driver.questions().unwrap().len(), 1);
    assert!(driver.answered_question("q2").unwrap().is_none());
}

#[tokio::test]
async fn a_damaged_store_that_loses_the_copy_of_its_link_says_to_unlink_it_by_its_space() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    drop(Driver::open(paths.clone()).unwrap());
    garble(&paths, SPACE);
    drop(Driver::open(paths.clone()).unwrap());
    let store = paths.store(SPACE).unwrap();
    let record = store.with_file_name("lifecycle.json");
    let saved = fs::read(&record).unwrap();
    fs::write(crate::link_copy::path(&store), b"{").unwrap();
    failures::take_counted_here();

    let listed = Driver::linked_folders(&paths).unwrap();
    let problem = listed[0].problem.clone().unwrap();
    assert!(
        problem.ends_with(&format!(
            "run koi folder unlink --space {SPACE}, then link the folder again"
        )),
        "{problem}"
    );
    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(driver.status().folders[0].problem.as_ref(), Some(&problem));
    assert!(failures::take_counted_here().is_empty());
    assert_eq!(
        fs::read(&record).unwrap(),
        saved,
        "the halt keeps when it began"
    );
}

#[test]
fn a_folder_whose_state_cannot_be_read_is_listed_as_halted() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block reads");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    drop(Driver::open(paths.clone()).unwrap());
    let space = paths.store(SPACE).unwrap().parent().unwrap().to_path_buf();
    fs::set_permissions(&space, fs::Permissions::from_mode(0o000)).unwrap();
    let listed = Driver::linked_folders(&paths);
    fs::set_permissions(&space, fs::Permissions::from_mode(0o700)).unwrap();
    let listed = listed.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, FolderState::Halted);
}

#[tokio::test(start_paused = true)]
async fn a_sign_out_the_server_never_answers_is_reported_by_every_sign_out_until_the_next_pairing()
{
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}/", silent.local_addr().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(&dir.path().join("profile"));
    Credential {
        server: server.clone(),
        ..credential("d1")
    }
    .save_new(&paths.credential())
    .unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    let unconfirmed = Unconfirmed {
        server,
        device_id: "d1".into(),
        reason: "no answer within 10 seconds".into(),
    };

    let left = driver.sign_out(true).await.unwrap();
    let again = driver.sign_out(true).await.unwrap();
    drop(silent);

    assert_eq!(left.unconfirmed.as_ref(), Some(&unconfirmed));
    assert_eq!(again.unconfirmed, Some(unconfirmed));
    assert!(!paths.credential().exists());
    driver.save_credential(credential("d2")).await.unwrap();
    assert!(!paths.unconfirmed_revocation().exists());
}

fn unplaced(paths: &Paths, space_id: &str) -> PathBuf {
    let store = paths.store(space_id).unwrap();
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    fs::write(&store, b"these bytes are no database").unwrap();
    store.parent().unwrap().to_path_buf()
}

#[tokio::test]
async fn signing_out_also_forgets_a_folder_whose_path_is_unknown() {
    const LOST: &str = "0190f0a0-0000-7000-8000-0000000000f1";
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let lost = unplaced(&paths, LOST);
    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(driver.status().folders.len(), 2);

    let left = driver.sign_out(true).await.unwrap();
    assert!(left.unconfirmed.is_some_and(|why| !why.reason.is_empty()));

    assert!(driver.status().folders.is_empty());
    assert!(!lost.exists());
    drop(driver);
    assert!(Driver::linked_folders(&paths).unwrap().is_empty());
}

#[tokio::test]
async fn replacing_a_lost_credential_reports_the_versions_its_folders_had_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    parked_report(&paths, &root);
    fs::remove_file(paths.credential()).unwrap();
    let date = koidrive_fs::utc_date(SystemTime::now());
    let driver = Driver::open(paths).unwrap();

    let (account, set_aside) = driver.save_credential(credential("d2")).await.unwrap();
    if koidrive_fs::utc_date(SystemTime::now()) != date {
        eprintln!("SKIPPED: the UTC date changed during the test");
        return;
    }

    let back = root.join(format!("report (recovered {date}).docx"));
    assert_eq!(account.device_id, "d2");
    assert_eq!(
        set_aside,
        SetAside {
            brought_back: vec![back.clone()],
            kept: Vec::new(),
        }
    );
    assert_eq!(fs::read(back).unwrap(), b"parked");
}

#[tokio::test]
async fn saving_a_credential_reports_what_a_pending_sign_out_brought_back_before_a_cleanup_failed()
{
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    let other_id = "0190f0a0-0000-7000-8000-000000000002";
    linked(&paths, &root, "d1");
    linked_to(&paths, &base.join("Other"), "d1", other_id);
    parked_report(&paths, &root);
    let driver = Driver::open(paths.clone()).unwrap();
    let links = (driver.inner.folders.lock().unwrap().values())
        .map(|folder| (&folder.link).into())
        .collect();
    let mut leaving = Leaving::begin(
        &paths.state_dir,
        Intent {
            links,
            ..Default::default()
        },
    )
    .unwrap();
    leaving.set_phase(Phase::Committed).unwrap();
    drop(driver);
    fs::remove_file(paths.credential()).unwrap();
    let spaces = paths.spaces();
    let date = koidrive_fs::utc_date(SystemTime::now());
    let driver = Driver::open(paths).unwrap();
    if !close_dir(&spaces) {
        return;
    }

    let failed = driver.save_credential(credential("d2")).await;
    open_dir(&spaces);
    let (_, set_aside) = driver.save_credential(credential("d2")).await.unwrap();
    if koidrive_fs::utc_date(SystemTime::now()) != date {
        eprintln!("SKIPPED: the UTC date changed during the test");
        return;
    }

    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(
        set_aside.brought_back,
        [root.join(format!("report (recovered {date}).docx"))]
    );
}

#[tokio::test]
async fn saving_another_credential_forgets_a_folder_whose_path_is_unknown() {
    const LOST: &str = "0190f0a0-0000-7000-8000-0000000000f2";
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let lost = unplaced(&paths, LOST);
    fs::remove_file(paths.credential()).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();

    driver.save_credential(credential("d2")).await.unwrap();

    assert!(driver.status().folders.is_empty());
    assert!(!lost.exists());
}

#[tokio::test]
async fn linking_a_space_whose_folder_path_is_unknown_asks_to_unlink_it_first() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    fs::create_dir(&root).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let lost = unplaced(&paths, SPACE);
    let spaces = prost::Message::encode_to_vec(&koidrive_proto::v1::SpaceList {
        spaces: vec![SpaceInfo {
            space_id: SPACE.into(),
            name: "Projects".into(),
            format_version: 1,
            ..Default::default()
        }],
        ..Default::default()
    });
    let (server, served) =
        serve_each(vec![access_token(), (200, spaces)], paths.credential()).await;
    Credential {
        server,
        ..credential("d1")
    }
    .save_new(&paths.credential())
    .unwrap();
    let driver = Driver::open(paths.clone()).unwrap();

    let refused = driver.link(&root, SPACE).await.unwrap_err();

    assert!(
        matches!(&refused, DriverError::SpaceUnplaced(space_id) if space_id == SPACE),
        "{refused}"
    );
    served.await.unwrap();
    assert_eq!(driver.status().folders.len(), 1);
    assert!(lost.join("store.sqlite").exists());
    assert_eq!(claim::owner(&root).unwrap(), None);
}

#[tokio::test]
async fn a_committed_sign_out_forgets_a_folder_whose_path_is_unknown_after_a_restart() {
    const LOST: &str = "0190f0a0-0000-7000-8000-0000000000f3";
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let lost = unplaced(&paths, LOST);
    let mut leaving = Leaving::begin(
        &paths.state_dir,
        Intent {
            unplaced: vec![LOST.into()],
            ..Default::default()
        },
    )
    .unwrap();
    leaving.set_phase(Phase::Committed).unwrap();

    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(driver.sign_out(false).await.unwrap().not_removed, []);
    assert!(!lost.exists());
    assert!(!paths.state_dir.join("leaving").exists());
}

#[tokio::test]
async fn signing_out_changes_nothing_while_a_folder_whose_path_is_unknown_cannot_be_forgotten() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block writes");
        return;
    }
    const LOST: &str = "0190f0a0-0000-7000-8000-0000000000f4";
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let lost = unplaced(&paths, LOST);
    let driver = Driver::open(paths.clone()).unwrap();
    fs::set_permissions(&lost, fs::Permissions::from_mode(0o500)).unwrap();
    let refused = driver.sign_out(true).await;
    fs::set_permissions(&lost, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(
        matches!(&refused, Err(DriverError::UnlinkBlocked { path, .. }) if *path == lost),
        "{refused:?}"
    );
    assert!(paths.credential().exists());
    assert!(!paths.state_dir.join("leaving").exists());
    assert_eq!(driver.status().folders.len(), 2);
}

fn garble(paths: &Paths, space_id: &str) {
    let store = paths.store(space_id).unwrap();
    for log in ["-wal", "-shm"] {
        let mut path = store.clone().into_os_string();
        path.push(log);
        let _ = fs::remove_file(path);
    }
    fs::write(&store, b"these bytes are no database").unwrap();
}

fn from_a_newer_build(paths: &Paths, space_id: &str) {
    rusqlite::Connection::open(paths.store(space_id).unwrap())
        .unwrap()
        .execute(
            "INSERT INTO migrations (name) VALUES ('9999_from_a_newer_build')",
            [],
        )
        .unwrap();
}

fn records(spaces: &Path) -> Vec<(PathBuf, SystemTime, Vec<u8>)> {
    (snapshot(spaces).into_iter())
        .filter(|(path, ..)| {
            path.file_name()
                .is_some_and(|name| name == "lifecycle.json" || name == "link.json")
        })
        .collect()
}

#[tokio::test]
async fn a_damaged_store_halts_only_its_folder_and_counts_once() {
    use crate::health::tests::{named, posthog, with_prefix};

    const GARBLED: &str = "0190f0a0-0000-7000-8000-0000000000c1";
    const NEWER: &str = "0190f0a0-0000-7000-8000-0000000000c2";
    const HEALTHY: &str = "0190f0a0-0000-7000-8000-0000000000c3";
    const EMPTIED: &str = "0190f0a0-0000-7000-8000-0000000000c4";
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    paired(&paths, "d1");
    let roots = [GARBLED, NEWER, HEALTHY, EMPTIED].map(|space_id| {
        let root = base.join(space_id);
        linked_to(&paths, &root, "d1", space_id);
        root
    });
    drop(Driver::open(paths.clone()).unwrap());
    garble(&paths, GARBLED);
    from_a_newer_build(&paths, NEWER);
    let emptied = paths.store(EMPTIED).unwrap();
    fs::write(&emptied, b"").unwrap();
    let halted = Failure::Halted(Halted::Store);
    let counted = || {
        (failures::take_counted_here().get(&halted))
            .copied()
            .unwrap_or(0)
    };
    counted();
    let folder = |folders: &[FolderStatus], space_id: &str| {
        (folders.iter())
            .find(|folder| folder.space_id == space_id)
            .unwrap()
            .clone()
    };

    let before = snapshot(&paths.spaces());
    let listed = Driver::linked_folders(&paths).unwrap();
    assert_eq!(
        snapshot(&paths.spaces()),
        before,
        "a listing writes nothing"
    );
    assert_eq!(counted(), 0, "a listing counts no halt");
    assert_eq!(listed.len(), 4);
    for (space_id, root) in [(GARBLED, &roots[0]), (EMPTIED, &roots[3])] {
        let broken = folder(&listed, space_id);
        assert_eq!(broken.state, FolderState::Halted);
        assert_eq!(broken.path.as_deref(), Some(root.as_path()));
        let problem = broken.problem.unwrap();
        assert!(
            problem.starts_with("the folder's sync database \"")
                && problem.contains("store.sqlite\" can't be opened (")
                && problem.ends_with(
                    "); run koi folder restart to try again, or unlink the folder and link it again"
                ),
            "{problem}"
        );
    }
    assert_eq!(folder(&listed, NEWER).state, FolderState::Halted);
    assert_ne!(folder(&listed, HEALTHY).state, FolderState::Halted);

    drop(Driver::open(paths.clone()).unwrap());
    assert_eq!(counted(), 3);
    assert_eq!(
        fs::metadata(&emptied).unwrap().len(),
        0,
        "an emptied store stays as it is"
    );
    let saved = records(&paths.spaces());
    let recorder = koidrive_telemetry::Recorder::on();
    let driver = Driver::open_with_telemetry(paths.clone(), recorder.clone()).unwrap();
    assert_eq!(counted(), 0, "a halt is counted once");
    assert_eq!(
        records(&paths.spaces()),
        saved,
        "a saved halt is not saved again"
    );
    driver.start();
    eventually("the healthy folder is claimed", || {
        claim::owner(&roots[2]).unwrap().is_some()
    })
    .await;
    assert!(claim::owner(&roots[0]).unwrap().is_none());
    assert!(claim::owner(&roots[1]).unwrap().is_none());
    assert!(claim::owner(&roots[3]).unwrap().is_none());
    let newer = folder(&driver.status().folders, NEWER).problem.unwrap();
    assert!(
        newer.contains("was updated by a newer Koidrive build")
            && newer.ends_with("install that build again to sync this folder"),
        "{newer}"
    );
    assert!(driver.questions().unwrap().is_empty());
    driver.record_health();
    let events = posthog(recorder.take());
    let report = named(&events, "hourly_report")[0];
    assert_eq!(with_prefix(report, "l.halted."), [("l.halted.store", 3)]);
    driver.stop().await;
}

#[tokio::test]
async fn a_restart_retries_a_damaged_store_and_syncs_once_it_opens() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    drop(Driver::open(paths.clone()).unwrap());
    let store = paths.store(SPACE).unwrap();
    let link = crate::link_copy::read(&store, SPACE).unwrap();
    garble(&paths, SPACE);
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    let halt = driver.status().folders[0].problem.clone().unwrap();
    failures::take_counted_here();

    driver.restart(&root).await.unwrap();
    assert_eq!(halted(&driver).await, halt);
    assert_eq!(
        failures::take_counted_here(),
        BTreeMap::from([(Failure::Halted(Halted::Store), 1)])
    );
    assert!(claim::owner(&root).unwrap().is_none());

    garble(&paths, SPACE);
    fs::remove_file(&store).unwrap();
    Store::open(&store)
        .unwrap()
        .transaction(|tx| {
            tx.set_link(&link)?;
            tx.put_question(&StoredQuestion {
                question_id: "q1".into(),
                asked_at_ms: 10,
                body: serde_json::to_vec(&mass_delete("q1")).unwrap(),
                answer: None,
            })
        })
        .unwrap();
    driver.restart(&root).await.unwrap();
    eventually("the restarted folder reads its store", || {
        let folder = &driver.status().folders[0];
        folder.state != FolderState::Halted && folder.questions == 1
    })
    .await;
    eventually("the restarted folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;
    assert_eq!(driver.questions().unwrap().len(), 1);
    driver.stop().await;
}

#[tokio::test]
async fn unlinking_a_folder_whose_store_is_damaged_forgets_it_and_keeps_its_files() {
    for by_space in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Koidrive");
        linked(&paths, &root, "d1");
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        eventually("the folder is claimed", || {
            claim::owner(&root).unwrap().is_some()
        })
        .await;
        driver.stop().await;
        drop(driver);
        fs::write(root.join("notes.txt"), b"kept").unwrap();
        let parked = root.join(".koidrive/parked/n1");
        fs::create_dir_all(parked.parent().unwrap()).unwrap();
        fs::write(&parked, b"parked").unwrap();
        garble(&paths, SPACE);

        let driver = Driver::open(paths.clone()).unwrap();
        let (unlinked, set_aside) = match by_space {
            true => driver.unlink_space(SPACE).await.unwrap().unwrap(),
            false => driver.unlink(&root).await.unwrap(),
        };
        assert_eq!(unlinked.root, root);
        assert_eq!(set_aside.kept, std::slice::from_ref(&parked));
        assert!(set_aside.brought_back.is_empty());
        assert_eq!(fs::read(&parked).unwrap(), b"parked");
        assert!(driver.status().folders.is_empty());
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
        assert!(claim::owner(&root).unwrap().is_none());
        assert_eq!(fs::read(root.join("notes.txt")).unwrap(), b"kept");
        drop(driver);
        assert!(Driver::linked_folders(&paths).unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_damaged_store_without_a_copy_of_its_link_shows_by_its_space_and_unlinks_by_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    garble(&paths, SPACE);

    let listed = Driver::linked_folders(&paths).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].space_id.as_str(), listed[0].path.as_deref()),
        (SPACE, None)
    );
    assert_eq!(listed[0].state, FolderState::Halted);
    let problem = listed[0].problem.clone().unwrap();
    assert!(
        problem.ends_with(&format!(
            "and the folder's path is unknown; run koi folder unlink --space {SPACE}, then link the folder again"
        )),
        "{problem}"
    );
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    assert_eq!(driver.status().folders, listed);
    assert!(matches!(
        driver.restart(&root).await,
        Err(DriverError::NotLinked(_))
    ));
    assert!(claim::owner(&root).unwrap().is_none());

    assert!(matches!(
        driver.unlink_space("../x").await,
        Err(DriverError::Paths(PathsError::BadSpaceId(_)))
    ));
    assert!(matches!(
        driver
            .unlink_space("0190f0a0-0000-7000-8000-0000000000ff")
            .await,
        Err(DriverError::SpaceNotLinked(_))
    ));
    assert_eq!(driver.status().folders, listed);
    assert_eq!(driver.unlink_space(SPACE).await.unwrap(), None);
    assert!(driver.status().folders.is_empty());
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    assert!(matches!(
        driver.unlink_space(SPACE).await,
        Err(DriverError::SpaceNotLinked(_))
    ));
    driver.stop().await;
}

#[test]
fn a_store_linked_before_links_had_a_copy_gets_one_when_the_profile_opens() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let store = paths.store(SPACE).unwrap();
    let copy = crate::link_copy::path(&store);

    Driver::linked_folders(&paths).unwrap();
    assert!(!copy.exists(), "a listing writes no copy");
    drop(Driver::open(paths.clone()).unwrap());
    let link = (Store::open(&store).unwrap())
        .transaction(|tx| tx.link())
        .unwrap()
        .unwrap();
    assert_eq!(crate::link_copy::read(&store, SPACE).as_ref(), Some(&link));

    let stale = Link {
        space_name: "Renamed".into(),
        ..link.clone()
    };
    crate::link_copy::write(&store, &stale).unwrap();
    Driver::linked_folders(&paths).unwrap();
    assert_eq!(crate::link_copy::read(&store, SPACE), Some(stale));
    drop(Driver::open(paths.clone()).unwrap());
    assert_eq!(crate::link_copy::read(&store, SPACE), Some(link));
}

#[tokio::test]
async fn a_folder_whose_task_cannot_start_is_counted_as_halted() {
    let halted = Failure::Halted(Halted::Start);
    let before = failures::count(halted);
    let (_dir, paths, _root) = profile(&[]);
    credential("d2").save_new(&paths.credential()).unwrap();
    let driver = Driver::open(paths).unwrap();
    driver.start();
    assert_eq!(driver.status().folders[0].state, FolderState::Halted);
    assert!(failures::count(halted) > before);
    driver.stop().await;
}

#[tokio::test]
async fn a_restart_that_halts_at_once_returns_the_halt() {
    let (_dir, paths, root) = profile(&[]);
    credential("d2").save_new(&paths.credential()).unwrap();
    let driver = Driver::open(paths).unwrap();
    driver.start();
    let restarted = driver.restart(&root).await.unwrap();
    assert_eq!(restarted.state, FolderState::Halted);
    assert!(
        restarted.problem.as_ref().unwrap().contains("device d1"),
        "{restarted:?}"
    );
    driver.stop().await;
}

#[tokio::test]
async fn a_lift_with_nothing_refused_says_so_and_restarts_the_folder() {
    let (_dir, paths, root) = profile(&[]);
    credential("d2").save_new(&paths.credential()).unwrap();
    let driver = Driver::open(paths).unwrap();
    driver.start();
    let error = driver.lift(&root, Lift::Resend).await.unwrap_err();
    assert!(matches!(error, DriverError::NothingToLift), "{error}");
    let folder = &driver.status().folders[0];
    assert_eq!(folder.state, FolderState::Halted);
    assert!(
        folder.problem.as_ref().unwrap().contains("device d1"),
        "{folder:?}"
    );
    driver.stop().await;
}
