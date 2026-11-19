// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn a_credential_saved_while_syncs_fail_after_publication_reports_what_was_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    koidrive_state_file::create_dir(&paths.state_dir, &paths.state_dir).unwrap();
    let set_aside = SetAside {
        brought_back: vec![dir.path().join("report (recovered).docx")],
        kept: Vec::new(),
    };
    record_unreported_set_aside(&paths.unreported_set_aside(), &set_aside);
    let driver = Driver::open(paths.clone()).unwrap();

    let failing = koidrive_state_file::fail_syncs_after_publication();
    let saved = driver.save_credential(credential("d2")).await;
    drop(failing);

    let running = driver.status().account;
    drop(driver);
    assert_eq!(
        running,
        Driver::open(paths.clone()).unwrap().status().account
    );
    assert_eq!(saved.unwrap(), (running.unwrap(), set_aside));
    assert!(!paths.unreported_set_aside().exists());
}

#[tokio::test]
async fn a_claim_whose_syncs_fail_after_publication_forgets_its_pending_create() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    koidrive_state_file::create_dir(&paths.state_dir, &paths.state_dir).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        write_reply(&mut socket, &issued("d1")).await;
    });

    let failing = koidrive_state_file::fail_syncs_after_publication();
    let claimed = driver.claim(&server, "claim-code", "Ada", "laptop").await;
    drop(failing);
    served.abort();

    let url = url::Url::parse(&server).unwrap();
    assert!(!crate::claim_pending(&paths, &url));
    assert_eq!(crate::pending_create::saved(&paths), 0);
    assert_eq!(claimed.unwrap().0.device_id, "d1");
}

#[tokio::test]
async fn a_restart_whose_sync_fails_after_publication_starts_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let other = Owner {
        server: nowhere().into(),
        device_id: "d2".into(),
    };
    claim::set_owner(&root, &other).unwrap();
    let driver = Driver::open(paths).unwrap();
    driver.start();
    halted(&driver).await;
    claim::release(&root, &other).unwrap().durable.unwrap();

    let failing = koidrive_state_file::fail_syncs_after_publication();
    let restarted = driver.restart(&root).await;
    drop(failing);
    let claimed = tokio::time::timeout(
        Duration::from_secs(10),
        eventually("the restarted folder is claimed", || {
            claim::owner(&root).unwrap().is_some()
        }),
    )
    .await;
    driver.stop().await;

    assert!(claimed.is_ok());
    assert!(restarted.is_ok(), "{restarted:?}");
}

#[tokio::test]
async fn an_unlink_whose_sync_fails_after_its_store_moves_away_finishes() {
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

    let failing = failing_syncs_of(&paths.spaces());
    let unlinked = driver.unlink(&root).await;
    drop(failing);

    let running = driver.status().folders.len();
    driver.stop().await;
    drop(driver);
    assert_eq!(running, Driver::open(paths).unwrap().status().folders.len());
    assert!(unlinked.is_ok(), "{:?}", unlinked.err());
}

#[tokio::test]
async fn an_unlink_whose_owner_record_sync_fails_keeps_the_folder_until_it_runs_again() {
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
    let store = paths.store(SPACE).unwrap();

    let failing = failing_syncs_of(claim::record(&root).parent().unwrap());
    let unlinked = driver.unlink(&root).await;
    drop(failing);

    let running = driver.status().folders;
    driver.stop().await;
    drop(driver);
    let reopened = Driver::open(paths).unwrap();
    let states = |folders: &[FolderStatus]| folders.iter().map(|f| f.state).collect::<Vec<_>>();
    assert_eq!(states(&running), [FolderState::Halted]);
    assert_eq!(states(&reopened.status().folders), [FolderState::Halted]);
    assert!(store.exists());
    assert!(unlinked.is_err());
    reopened.unlink(&root).await.unwrap();
    assert!(reopened.status().folders.is_empty());
    assert!(!claim::record(&root).exists());
    assert!(!store.parent().unwrap().exists());
}

#[tokio::test]
async fn a_refused_sign_out_whose_syncs_fail_after_publication_keeps_what_a_restart_reads() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let credential_path = Paths::under(&base.join("profile")).credential();
    let (server, _served) =
        serve_each(vec![access_token(), last_device_refused()], credential_path).await;
    let paths = linked_with_server(&base, server);
    let driver = Driver::open(paths.clone()).unwrap();

    let failing = koidrive_state_file::fail_syncs_after_publication();
    let refused = driver.sign_out(false).await;
    drop(failing);

    let running = driver.status();
    drop(driver);
    let reopened = Driver::open(paths).unwrap().status();
    assert_eq!(running.account, reopened.account);
    assert_eq!(running.folders.len(), reopened.folders.len());
    assert!(
        matches!(refused, Err(DriverError::LastDevice)),
        "{refused:?}"
    );
}

fn failing_syncs_of(dir: &Path) -> CleanupSyncProbe {
    let target = fs::metadata(dir).unwrap();
    let target = (target.dev(), target.ino());
    crate::leave::SYNC_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |dir| {
            let found = dir.metadata()?;
            match (found.dev(), found.ino()) == target {
                true => Err(io::Error::other("injected sync failure")),
                false => Ok(()),
            }
        }))
    });
    CleanupSyncProbe
}

#[tokio::test]
async fn a_sign_out_whose_store_removal_fails_runs_as_a_reopen_reads_until_a_retry_finishes() {
    use std::os::unix::fs::PermissionsExt;
    const LOST: &str = "0190f0a0-0000-7000-8000-0000000000f6";
    let seen = |driver: &Driver| {
        let status = driver.status();
        let folders: Vec<_> = status.folders.into_iter().map(|f| f.space_id).collect();
        let pairable = driver.check_unpaired().is_ok();
        (
            status.account,
            folders,
            status.running,
            status.paused,
            pairable,
        )
    };
    let mut disagreements = Vec::new();
    for renames in [false, true] {
        if !renames && rustix::process::geteuid().is_root() {
            continue;
        }
        for (link, lost) in [(true, false), (false, true), (true, true)] {
            let dir = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(dir.path()).unwrap();
            let paths = Paths::under(&base.join("profile"));
            paired(&paths, "d1");
            let mut ids = Vec::new();
            if link {
                linked_to(&paths, &base.join("Koidrive"), "d1", SPACE);
                ids.push(SPACE);
            }
            if lost {
                unplaced(&paths, LOST);
                ids.push(LOST);
            }
            let driver = Driver::open(paths.clone()).unwrap();
            let intent = Intent {
                links: (driver.inner.folders.lock().unwrap().values())
                    .map(|folder| (&folder.link).into())
                    .collect(),
                unplaced: driver
                    .inner
                    .unplaced
                    .lock()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect(),
                ..Default::default()
            };
            let mut leaving = Leaving::begin(&paths.state_dir, intent).unwrap();
            leaving.set_phase(Phase::Committed).unwrap();
            drop(leaving);

            let mode = |mode| fs::set_permissions(paths.spaces(), fs::Permissions::from_mode(mode));
            let failing = (!renames).then(|| mode(0o555).unwrap());
            let failing_sync = renames.then(|| failing_syncs_of(&paths.spaces()));
            let failed = driver.sign_out(false).await.unwrap().not_removed;
            drop(failing_sync);
            if failing.is_some() {
                mode(0o755).unwrap();
            }
            for id in &ids {
                let store = paths.spaces().join(id);
                assert!(failed.iter().any(|f| f.path == store), "{failed:?}");
            }
            let cell = format!("renames={renames} link={link} unplaced={lost}");
            let running = seen(&driver);
            drop(driver);
            let driver = Driver::open(paths.clone()).unwrap();
            if running != seen(&driver) {
                disagreements.push(format!("{cell}: {running:?} != {:?}", seen(&driver)));
            }

            assert_eq!(driver.sign_out(false).await.unwrap().not_removed, []);
            let retried = seen(&driver);
            drop(driver);
            assert_eq!(retried, seen(&Driver::open(paths.clone()).unwrap()));
            assert_eq!(retried, (None, Vec::new(), false, false, true), "{cell}");
            assert!(ids.iter().all(|id| !paths.spaces().join(id).exists()));
        }
    }
    assert!(disagreements.is_empty(), "{disagreements:#?}");
}
