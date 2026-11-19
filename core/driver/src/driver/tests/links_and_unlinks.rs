// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn no_task_starts_while_an_unlink_is_unfinished() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    let unlink = tokio::spawn({
        let driver = driver.clone();
        let root = root.clone();
        async move { driver.unlink(&root).await }
    });
    assert!(still_waiting(&unlink).await);
    unlink.abort();
    assert!(unlink.await.unwrap_err().is_cancelled());
    gate.open();
    driver.stop().await;
    assert!(lockable(&root));

    driver.start();
    let folder = &driver.status().folders[0];
    assert_eq!(
        folder.state,
        FolderState::Halted,
        "a task started for a folder whose unlink is unfinished"
    );
    assert!(folder.problem.as_ref().unwrap().contains("unlink it again"));
    driver.sign_out(true).await.unwrap();
    signed_out(&driver, &paths, &root);
}

#[tokio::test]
async fn an_unlink_preparation_failure_leaves_the_task_running() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        let (driver, gate) = paused_driver(&paths, &root).await;
        let blocked = paths
            .store(SPACE)
            .unwrap()
            .with_file_name("lifecycle.json.tmp");
        fs::create_dir(&blocked).unwrap();
        let watch = driver.watch();
        assert!(matches!(
            driver.unlink(&root).await,
            Err(DriverError::Io { .. })
        ));
        assert!(watch.has_changed().unwrap());
        {
            let folders = driver.inner.folders.lock().unwrap();
            let folder = folders.get(SPACE).unwrap();
            assert!(folder.unlink.is_none());
            assert!(!*folder.run.as_ref().unwrap().stop.borrow());
            assert!(!folder.status.needs_restart());
        }
        assert!(!lockable(&root));
        fs::remove_dir(blocked).unwrap();
        gate.open();
        driver.unlink(&root).await.unwrap();
        assert!(driver.status().folders.is_empty());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_process_exit_during_unlink_keeps_its_recovery_authority() {
    const CHILD: &str = "KOI_UNLINK_EXIT_CHILD";
    if let Some(base) = std::env::var_os(CHILD) {
        let base = PathBuf::from(base);
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Koidrive");
        let (driver, _gate) = paused_driver(&paths, &root).await;
        let unlink = tokio::spawn({
            let driver = driver.clone();
            async move { driver.unlink(&root).await }
        });
        assert!(still_waiting(&unlink).await);
        std::process::exit(0);
    }
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "driver::tests::a_process_exit_during_unlink_keeps_its_recovery_authority",
            "--nocapture",
        ])
        .env(CHILD, &base)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(15), child.wait()).await;
    let Ok(result) = result else {
        let _ = child.kill().await;
        panic!("the unlink child did not exit within 15 seconds");
    };
    assert!(result.unwrap().success());
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    assert_eq!(driver.status().folders[0].state, FolderState::Halted);
    driver.restart(&root).await.unwrap();
    assert_eq!(driver.status().folders[0].state, FolderState::Halted);
    assert!(lockable(&root));
    driver.unlink(&root).await.unwrap();
    assert!(driver.status().folders.is_empty());
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
}

#[tokio::test]
async fn an_unfinished_unlink_stays_halted_after_reopen_resume_and_restart() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        let (driver, gate) = paused_driver(&paths, &root).await;
        let unlink = tokio::spawn({
            let driver = driver.clone();
            let root = root.clone();
            async move { driver.unlink(&root).await }
        });
        assert!(still_waiting(&unlink).await);
        unlink.abort();
        assert!(unlink.await.unwrap_err().is_cancelled());
        let lifecycle = driver
            .inner
            .folders
            .lock()
            .unwrap()
            .get(SPACE)
            .unwrap()
            .status
            .clone();
        lifecycle.halt(
            Halted::Io,
            "a task stopped after unlink was cancelled".into(),
            crate::status::Remedy::Restart,
        );
        assert!(
            lifecycle
                .claimed(claim::Identity::of(&fs::metadata(&root).unwrap()))
                .is_err()
        );
        gate.open();
        driver.pause().await.unwrap();
        drop(driver);

        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        assert!(driver.status().paused);
        assert_eq!(driver.status().folders[0].state, FolderState::Halted);
        driver.resume().await.unwrap();
        driver.restart(&root).await.unwrap();
        assert!(lockable(&root));
        let status = driver.status();
        assert_eq!(status.folders[0].state, FolderState::Halted);
        assert!(
            status.folders[0]
                .problem
                .as_ref()
                .unwrap()
                .contains("unlink it again")
        );
        driver.unlink(&root).await.unwrap();
        assert!(driver.status().folders.is_empty());
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_pause_cannot_report_success_until_its_halt_is_saved() {
    let (_dir, paths, _root) = profile(&[]);
    let temporary = paths
        .store(SPACE)
        .unwrap()
        .with_file_name("lifecycle.json.tmp");
    fs::create_dir(&temporary).unwrap();
    credential("d2").save_new(&paths.credential()).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    assert!(matches!(driver.pause().await, Err(DriverError::Io { .. })));
    fs::remove_dir(&temporary).unwrap();
    assert!(!driver.pause().await.unwrap());
    let problem = driver.status().folders[0].problem.clone();
    drop(driver);
    let reopened = Driver::open(paths).unwrap();
    assert!(reopened.status().paused);
    assert_eq!(reopened.status().folders[0].state, FolderState::Halted);
    assert_eq!(reopened.status().folders[0].problem, problem);
}

#[tokio::test]
async fn a_start_during_a_sign_out_leaves_the_driver_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    let sign_out = tokio::spawn({
        let driver = driver.clone();
        async move { driver.sign_out(true).await }
    });
    eventually("the sign-out stops the driver", || !driver.status().running).await;
    driver.start();

    gate.open();
    sign_out.await.unwrap().unwrap();
    signed_out(&driver, &paths, &root);
}

#[tokio::test]
async fn a_profile_restored_without_its_credential_pairs_as_a_new_device_and_links_its_folder_again()
 {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("notes.txt"), b"kept").unwrap();
    let paths = Paths::under(&base.join("profile"));
    let token = || {
        prost::Message::encode_to_vec(&koidrive_proto::v1::AccessToken {
            token: "token".into(),
            expires_at_ms: i64::MAX,
            ..Default::default()
        })
    };
    let spaces = || {
        prost::Message::encode_to_vec(&koidrive_proto::v1::SpaceList {
            spaces: vec![SpaceInfo {
                space_id: SPACE.into(),
                name: "Projects".into(),
                ..Default::default()
            }],
            ..Default::default()
        })
    };
    let (server, held) = serve_held(vec![token(), spaces()]).await;
    held.release.send(()).unwrap();
    Credential {
        server,
        ..credential("d1")
    }
    .save_new(&paths.credential())
    .unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    driver.link(&root, "Projects").await.unwrap();
    drop(driver);
    fs::remove_file(paths.credential()).unwrap();

    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    let status = driver.status();
    assert_eq!(status.account, None);
    assert_eq!(status.folders[0].state, FolderState::Halted);
    let problem = status.folders[0].problem.clone().unwrap();
    assert!(problem.contains("signed out"), "{problem}");
    driver.stop().await;

    let (server, held) = serve_held(vec![issued("d2"), token(), spaces()]).await;
    held.release.send(()).unwrap();
    driver.pair(&server, "code", "Mac").await.unwrap();
    assert!(driver.status().folders.is_empty());
    assert_eq!(claim::owner(&root).unwrap(), None);
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    driver.link(&root, "Projects").await.unwrap();
    let status = driver.status();
    assert_eq!(status.account.unwrap().device_id, "d2");
    assert_eq!(status.folders.len(), 1);
    assert_eq!(fs::read(root.join("notes.txt")).unwrap(), b"kept");
    assert_eq!(
        held.served.await.unwrap(),
        ["/v1/pair/complete", "/v1/token", "/v1/spaces"]
    );
}

#[tokio::test]
async fn linking_refuses_a_folder_of_another_device_or_another_process() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    fs::create_dir(&root).unwrap();
    let paths = Paths::under(&base.join("profile"));
    paired(&paths, "d2");
    let driver = Driver::open(paths).unwrap();
    fs::create_dir(root.join(koidrive_fs::PRIVATE_DIR)).unwrap();
    let first = Owner {
        server: nowhere().into(),
        device_id: "d1".into(),
    };
    claim::set_owner(&root, &first).unwrap();
    assert!(matches!(
        driver.link(&root, "Projects").await,
        Err(DriverError::FolderOfOtherDevice { device_id, .. }) if device_id == "d1"
    ));

    claim::release(&root, &first).unwrap().durable.unwrap();
    let held = claim::lock(&root).unwrap().unwrap();
    assert!(matches!(
        driver.link(&root, "Projects").await,
        Err(DriverError::FolderInUse(_))
    ));
    drop(held);
    assert!(matches!(
        driver.link(&root, "Projects").await,
        Err(DriverError::Api(_))
    ));
    assert!(driver.status().folders.is_empty());
}

#[tokio::test]
async fn link_approval_rechecks_syncable_files_after_the_server_reply() {
    for (name, consent, allowed) in [
        (".DS_Store", false, true),
        ("THUMBS.DB", false, true),
        ("report.docx", false, false),
        ("report.docx", true, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("desktop.ini"), b"metadata").unwrap();
        let approval = LinkApproval {
            directory: crate::DirectoryIdentity::of(&fs::metadata(&root).unwrap()),
            accept_existing_files: consent,
        };
        let spaces = prost::Message::encode_to_vec(&koidrive_proto::v1::SpaceList {
            spaces: vec![SpaceInfo {
                space_id: SPACE.into(),
                name: "Projects".into(),
                format_version: 1,
                ..Default::default()
            }],
            ..Default::default()
        });
        let (server, held) = serve_held(vec![access_token().1, spaces]).await;
        let paths = Paths::under(&base.join("profile"));
        Credential {
            server,
            ..credential("d1")
        }
        .save_new(&paths.credential())
        .unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        let pending = tokio::spawn({
            let driver = driver.clone();
            let root = root.clone();
            async move { driver.link_approved(&root, SPACE, Some(approval)).await }
        });
        held.arrived.await.unwrap();
        assert!(claim::lock(&root).unwrap().is_none());
        fs::write(root.join(name), b"arrived after approval").unwrap();
        held.release.send(()).unwrap();
        let result = pending.await.unwrap();
        assert_eq!(held.served.await.unwrap(), ["/v1/token", "/v1/spaces"]);
        if allowed {
            assert!(
                matches!(result, Ok(Linked::New(_))),
                "{name}, consent={consent}: {result:?}"
            );
            assert_eq!(read_marker(&root).unwrap().unwrap().space_id, SPACE);
            assert_eq!(claim::owner(&root).unwrap().unwrap().device_id, "d1");
            assert_eq!(driver.status().folders.len(), 1);
        } else {
            assert!(
                matches!(result, Err(DriverError::FolderApprovalChanged(_))),
                "{name}: {result:?}"
            );
            assert!(read_marker(&root).unwrap().is_none());
            assert!(claim::owner(&root).unwrap().is_none());
            assert!(driver.status().folders.is_empty());
            assert!(!paths.store(SPACE).unwrap().exists());
        }
        assert_eq!(
            fs::read(root.join(name)).unwrap(),
            b"arrived after approval"
        );
    }
}

#[tokio::test]
async fn link_approval_keeps_identity_and_lock_guards_for_metadata_only_folders() {
    for consent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        fs::create_dir(&root).unwrap();
        fs::write(root.join(".DS_Store"), b"metadata").unwrap();
        let approval = LinkApproval {
            directory: crate::DirectoryIdentity::of(&fs::metadata(&root).unwrap()),
            accept_existing_files: consent,
        };
        let spaces = prost::Message::encode_to_vec(&koidrive_proto::v1::SpaceList {
            spaces: vec![SpaceInfo {
                space_id: SPACE.into(),
                name: "Projects".into(),
                format_version: 1,
                ..Default::default()
            }],
            ..Default::default()
        });
        let (server, held) = serve_held(vec![access_token().1, spaces]).await;
        let paths = Paths::under(&base.join("profile"));
        Credential {
            server,
            ..credential("d1")
        }
        .save_new(&paths.credential())
        .unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        let lock = claim::lock(&root).unwrap().unwrap();
        assert!(matches!(
            driver.link_approved(&root, SPACE, Some(approval)).await,
            Err(DriverError::FolderInUse(_))
        ));
        drop(lock);
        let pending = tokio::spawn({
            let driver = driver.clone();
            let root = root.clone();
            async move { driver.link_approved(&root, SPACE, Some(approval)).await }
        });
        held.arrived.await.unwrap();
        let original = base.join("original");
        fs::rename(&root, &original).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join(".DS_Store"), b"replacement metadata").unwrap();
        held.release.send(()).unwrap();
        assert!(matches!(
            pending.await.unwrap(),
            Err(DriverError::FolderApprovalChanged(_))
        ));
        assert_eq!(held.served.await.unwrap(), ["/v1/token", "/v1/spaces"]);
        for folder in [&root, &original] {
            assert!(!folder.join(".koidrive/space.json").exists());
            assert!(!folder.join(".koidrive/device.json").exists());
        }
        assert!(driver.status().folders.is_empty());
        assert!(!paths.store(SPACE).unwrap().exists());
    }
}

#[tokio::test]
async fn unlinking_forgets_the_store_and_keeps_the_files() {
    let (_dir, paths, root) = profile(&[]);
    fs::write(root.join("keep.txt"), b"mine").unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(matches!(
        driver.unlink(&root.join("elsewhere")).await,
        Err(DriverError::NotLinked(_))
    ));
    let (link, _) = driver.unlink(&root).await.unwrap();
    assert_eq!(link.space_id, SPACE);
    assert!(driver.status().folders.is_empty());
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"mine");
    drop(driver);
    assert!(Driver::open(paths).unwrap().status().folders.is_empty());
}

#[tokio::test]
async fn linking_a_folder_to_the_space_it_syncs_changes_nothing_and_to_another_is_refused() {
    let (_dir, paths, root) = profile(&[]);
    let token = prost::Message::encode_to_vec(&koidrive_proto::v1::AccessToken {
        token: "token".into(),
        expires_at_ms: i64::MAX,
        ..Default::default()
    });
    let spaces = prost::Message::encode_to_vec(&koidrive_proto::v1::SpaceList {
        spaces: vec![
            SpaceInfo {
                space_id: SPACE.into(),
                name: "Projects".into(),
                ..Default::default()
            },
            SpaceInfo {
                space_id: "0190f0a0-0000-7000-8000-000000000002".into(),
                name: "Notes".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    let (server, served) =
        serve_recorded(vec![(200, token), (200, spaces.clone()), (200, spaces)]).await;
    Store::open(paths.store(SPACE).unwrap())
        .unwrap()
        .transaction(|tx| {
            let link = tx.link()?.unwrap();
            tx.set_link(&Link {
                server_url: server.clone(),
                ..link
            })
        })
        .unwrap();
    Credential {
        server,
        ..credential("d1")
    }
    .save_new(&paths.credential())
    .unwrap();
    let driver = Driver::open(paths).unwrap();
    let before = driver.status().folders;

    assert_eq!(
        driver.link(&root, SPACE).await.unwrap(),
        Linked::Already(before[0].clone())
    );
    assert_eq!(
        driver.link(&root, "Projects").await.unwrap(),
        Linked::Already(before[0].clone())
    );
    assert!(matches!(
        driver.link(&root, "Notes").await,
        Err(DriverError::FolderOverlaps(path)) if path == root
    ));
    assert_eq!(driver.status().folders, before);
    assert_eq!(read_marker(&root).unwrap(), None);
    let paths: Vec<String> = (served.await.unwrap().into_iter())
        .map(|(path, _)| path)
        .collect();
    assert_eq!(paths, ["/v1/token", "/v1/spaces", "/v1/spaces"]);
}

#[tokio::test]
async fn linking_a_folder_another_device_linked_is_refused_and_changes_nothing() {
    let (_dir, paths, root) = profile(&[]);
    credential("d2").save_new(&paths.credential()).unwrap();
    let driver = Driver::open(paths).unwrap();
    let before = driver.status().folders;
    assert!(matches!(
        driver.link(&root, SPACE).await,
        Err(DriverError::LinkedByOtherDevice { device_id, .. }) if device_id == "d1"
    ));
    assert_eq!(driver.status().folders, before);
}

#[tokio::test]
async fn linking_needs_a_paired_device() {
    let dir = tempfile::tempdir().unwrap();
    let driver = Driver::open(Paths::under(&dir.path().join("profile"))).unwrap();
    assert!(matches!(
        driver.link(dir.path(), "Projects").await,
        Err(DriverError::NotPaired)
    ));
}

#[test]
fn a_folder_may_not_hold_or_sit_inside_the_profile_or_another_folder() {
    let (dir, paths, root) = profile(&[]);
    let base = fs::canonicalize(dir.path()).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    let folders = driver.inner.folders.lock().unwrap();
    let check = |path: &Path| driver.check_placement(path, &folders);
    assert!(matches!(
        check(&base),
        Err(DriverError::FolderHoldsProfile(_))
    ));
    assert!(matches!(
        check(&paths.state_dir.join("spaces")),
        Err(DriverError::FolderHoldsProfile(_))
    ));
    assert!(matches!(
        check(&root.join("inner")),
        Err(DriverError::FolderOverlaps(_))
    ));
    check(&base.join("Other")).unwrap();
}

#[test]
fn spaces_resolve_by_id_then_by_unique_name() {
    let space = |id: &str, name: &str| SpaceInfo {
        space_id: id.into(),
        name: name.into(),
        ..Default::default()
    };
    let spaces = vec![
        space("a1", "Work"),
        space("b2", "Home"),
        space("c3", "Home"),
        space("d4", "a1"),
    ];
    assert_eq!(resolve_space(spaces.clone(), "a1").unwrap().space_id, "a1");
    assert_eq!(
        resolve_space(spaces.clone(), "Work").unwrap().space_id,
        "a1"
    );
    assert!(matches!(
        resolve_space(spaces.clone(), "Home"),
        Err(DriverError::SpaceAmbiguous { ids, .. }) if ids == ["b2", "c3"]
    ));
    assert!(matches!(
        resolve_space(spaces, "work"),
        Err(DriverError::SpaceNotFound(_))
    ));
}
