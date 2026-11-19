// SPDX-License-Identifier: Apache-2.0

#[test]
fn cleanup_results_serialize_lossy_paths_without_changing_the_retry_path() {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let bytes = b"/tmp/cleanup-\xff\n\x1b[31m";
    let failure = CleanupFailure {
        path: PathBuf::from(OsString::from_vec(bytes.to_vec())),
        error: "denied\n\x1b[31m".into(),
    };
    let data =
        serde_json::to_value(&failure).expect("a cleanup result must serialize a non-UTF-8 path");
    assert_eq!(data["path"], failure.path.to_string_lossy().as_ref());
    assert_eq!(data["error"], failure.error);
    assert_eq!(failure.path.as_os_str().as_bytes(), bytes);
    let json = serde_json::to_string(&failure).unwrap();
    assert!(!json.contains('\n'));
    assert!(!json.contains('\x1b'));
}

fn owned(paths: &Paths, root: &Path) {
    let owner = Owner {
        server: nowhere().into(),
        device_id: "d1".into(),
    };
    claim::set_owner(root, &owner).unwrap();
    assert_eq!(claim::owner(root).unwrap(), Some(owner));
    assert!(paths.store(SPACE).unwrap().exists());
}

#[tokio::test]
async fn signing_out_changes_nothing_while_a_folder_cannot_be_unlinked() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let credential_path = Paths::under(&base.join("profile")).credential();
    let (server, served) = serve_each(Vec::new(), credential_path).await;
    let paths = linked_with_server(&base, server);
    owned(&paths, &root);
    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block reads");
        return;
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();

    for confirm_last_device in [false, true] {
        let blocked = driver.sign_out(confirm_last_device).await;
        assert!(
            matches!(&blocked, Err(DriverError::UnlinkBlocked { path, .. }) if *path == root),
            "{blocked:?}"
        );
        assert_eq!(driver.status().account.unwrap().device_id, "d1");
        assert_eq!(driver.status().folders.len(), 1);
        assert!(paths.credential().exists());
        assert!(paths.store(SPACE).unwrap().exists());
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(claim::owner(&root).unwrap().is_some());
    assert_eq!(served.await.unwrap(), []);
}

#[tokio::test]
async fn reviewer_a_refused_leave_does_not_keep_an_outdated_root_after_credential_loss() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let old = base.join("Old");
    let new = base.join("New");
    linked(&paths, &old, "d1");
    let owner = Owner {
        server: nowhere().into(),
        device_id: "d1".into(),
    };
    claim::set_owner(&old, &owner).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(matches!(
        driver.sign_out(false).await,
        Err(DriverError::NotRevoked(_))
    ));
    assert!(!paths.state_dir.join("leaving").exists());
    driver.unlink(&old).await.unwrap();
    drop(driver);
    linked_to(&paths, &new, "d1", SPACE);
    claim::set_owner(&new, &owner).unwrap();
    fs::remove_file(paths.credential()).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(driver.sign_out(true).await.unwrap().not_removed, []);
    assert_eq!(
        claim::owner(&new).unwrap(),
        None,
        "the current folder must be released, not only the old prepared root"
    );
}

#[tokio::test]
async fn a_prepared_sign_out_releases_every_current_root_and_owner_after_credential_loss() {
    for (same_root, device_id) in [(true, "d2"), (false, "d1")] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Koidrive");
        linked(&paths, &root, "d1");
        let old = Owner {
            server: nowhere().into(),
            device_id: "d1".into(),
        };
        claim::set_owner(&root, &old).unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        let links = driver
            .inner
            .folders
            .lock()
            .unwrap()
            .values()
            .map(|folder| (&folder.link).into())
            .collect();
        Leaving::begin(
            &paths.state_dir,
            Intent {
                links,
                ..Default::default()
            },
        )
        .unwrap();
        drop(driver);
        let current_root = if same_root {
            root.clone()
        } else {
            base.join("New")
        };
        linked_to(&paths, &current_root, device_id, SPACE);
        let current = Owner {
            device_id: device_id.into(),
            ..old
        };
        claim::set_owner(&current_root, &current).unwrap();
        fs::remove_file(paths.credential()).unwrap();

        let driver = Driver::open(paths.clone()).unwrap();
        assert_eq!(driver.sign_out(true).await.unwrap().not_removed, []);
        assert_eq!(claim::owner(&root).unwrap(), None);
        assert_eq!(claim::owner(&current_root).unwrap(), None);
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    }
}

#[tokio::test]
async fn committed_cleanup_preserves_a_new_owner_at_the_same_root() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    let driver = Driver::open(paths.clone()).unwrap();
    let links = driver
        .inner
        .folders
        .lock()
        .unwrap()
        .values()
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
    let replacement = Owner {
        server: nowhere().into(),
        device_id: "d2".into(),
    };
    claim::set_owner(&root, &replacement).unwrap();

    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(driver.sign_out(false).await.unwrap().not_removed, []);
    assert_eq!(claim::owner(&root).unwrap(), Some(replacement));
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
}

struct CleanupSyncProbe;

impl Drop for CleanupSyncProbe {
    fn drop(&mut self) {
        crate::leave::SYNC_HOOK.with(|hook| *hook.borrow_mut() = None);
    }
}

async fn cleanup_parent_sync_failure_is_retryable(target: usize) {
    use std::cell::{Cell, RefCell};
    use std::os::unix::fs::MetadataExt;
    use std::rc::Rc;

    for reopen in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Koidrive");
        let other = base.join("Other");
        let other_id = "0190f0a0-0000-7000-8000-000000000002";
        linked(&paths, &root, "d1");
        owned(&paths, &root);
        linked_to(&paths, &other, "d1", other_id);
        claim::set_owner(&other, &claim::owner(&root).unwrap().unwrap()).unwrap();
        fs::write(root.join("keep.txt"), b"keep").unwrap();
        fs::write(other.join("keep.txt"), b"keep too").unwrap();
        let mut driver = Driver::open(paths.clone()).unwrap();
        fs::write(next_path(&paths.credential()), b"unpromoted credential").unwrap();
        let links = driver
            .inner
            .folders
            .lock()
            .unwrap()
            .values()
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
        drop(leaving);
        let parents = [
            paths.state_dir.clone(),
            paths.spaces(),
            root.join(".koidrive"),
            other.join(".koidrive"),
        ];
        let identities: Vec<_> = parents
            .iter()
            .map(|path| {
                let metadata = fs::metadata(path).unwrap();
                (metadata.dev(), metadata.ino())
            })
            .collect();
        let fail = Rc::new(Cell::new(true));
        let events = Rc::new(RefCell::new(Vec::new()));
        let _probe = CleanupSyncProbe;
        crate::leave::SYNC_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new({
                let fail = fail.clone();
                let events = events.clone();
                let state = paths.state_dir.clone();
                let target = identities[target];
                move |dir| {
                    let metadata = dir.metadata()?;
                    let identity = (metadata.dev(), metadata.ino());
                    events
                        .borrow_mut()
                        .push((identity, state.join("leaving").exists()));
                    if fail.get() && identity == target {
                        return Err(io::Error::other("injected cleanup directory sync failure"));
                    }
                    Ok(())
                }
            }))
        });
        let failed = driver.sign_out(false).await.unwrap().not_removed;
        assert!(
            !failed.is_empty(),
            "sync failure was not reported for target {target}"
        );
        assert!(
            failed.iter().all(|failure| failure
                .error
                .contains("injected cleanup directory sync failure")),
            "{failed:?}"
        );
        assert!(
            matches!(
                crate::leave::phase(&paths.state_dir),
                Ok(Some(Phase::Committed))
            ),
            "sync failure lost committed intent for target {target}"
        );
        assert!(
            events.borrow().iter().all(|(_, recorded)| *recorded),
            "a cleanup sync ran after journal removal"
        );
        assert!(!paths.credential().exists());
        assert!(!next_path(&paths.credential()).exists());
        assert_eq!(claim::owner(&root).unwrap(), None);
        assert_eq!(claim::owner(&other).unwrap(), None);
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
        assert!(!paths.store(other_id).unwrap().parent().unwrap().exists());
        assert!(driver.status().account.is_none());
        assert!(driver.status().folders.is_empty());
        let boundary = events.borrow().len();
        fail.set(false);
        if reopen {
            drop(driver);
            driver = Driver::open(paths.clone()).unwrap();
        }
        assert_eq!(driver.sign_out(false).await.unwrap().not_removed, []);
        let events = events.borrow();
        let retry = &events[boundary..];
        let (last, cleanup) = retry.split_last().unwrap();
        assert_eq!(*last, (identities[0], false));
        assert!(cleanup.iter().all(|(_, recorded)| *recorded));
        for identity in &identities {
            assert!(
                cleanup.iter().any(|(seen, _)| seen == identity),
                "retry did not flush an already-removed cleanup entry"
            );
        }
        assert!(!paths.state_dir.join("leaving").exists());
        assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"keep");
        assert_eq!(fs::read(other.join("keep.txt")).unwrap(), b"keep too");
    }
}

#[tokio::test]
async fn a_failed_store_parent_sync_keeps_committed_cleanup_for_retry() {
    cleanup_parent_sync_failure_is_retryable(1).await;
}

#[tokio::test]
async fn a_failed_owner_parent_sync_keeps_committed_cleanup_for_retry() {
    cleanup_parent_sync_failure_is_retryable(2).await;
}

#[tokio::test]
async fn a_failed_credential_parent_sync_keeps_committed_cleanup_for_retry() {
    cleanup_parent_sync_failure_is_retryable(0).await;
}

#[tokio::test]
async fn absent_cleanup_parents_and_no_folders_do_not_block_sign_out() {
    use std::cell::RefCell;
    use std::os::unix::fs::MetadataExt;
    use std::rc::Rc;

    for linked_folder in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Koidrive");
        if linked_folder {
            linked(&paths, &root, "d1");
            owned(&paths, &root);
            fs::write(root.join("keep.txt"), b"keep").unwrap();
        }
        let driver = Driver::open(paths.clone()).unwrap();
        let links = driver
            .inner
            .folders
            .lock()
            .unwrap()
            .values()
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
        drop(leaving);
        drop(driver);
        if linked_folder {
            fs::remove_dir_all(root.join(".koidrive")).unwrap();
            fs::remove_dir_all(paths.spaces()).unwrap();
        }
        let events = Rc::new(RefCell::new(Vec::new()));
        let _probe = CleanupSyncProbe;
        crate::leave::SYNC_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new({
                let events = events.clone();
                let journal = paths.state_dir.join("leaving");
                move |dir| {
                    let metadata = dir.metadata()?;
                    events
                        .borrow_mut()
                        .push(((metadata.dev(), metadata.ino()), journal.exists()));
                    Ok(())
                }
            }))
        });
        let driver = Driver::open(paths.clone()).unwrap();
        assert_eq!(driver.sign_out(false).await.unwrap().not_removed, []);
        let metadata = fs::metadata(&paths.state_dir).unwrap();
        let state = (metadata.dev(), metadata.ino());
        let events = events.borrow();
        assert!(
            events.contains(&(state, true)),
            "credential parent was not flushed before journal removal"
        );
        assert_eq!(events.last(), Some(&(state, false)));
        if linked_folder {
            let metadata = fs::metadata(&root).unwrap();
            assert!(
                events.contains(&((metadata.dev(), metadata.ino()), true)),
                "missing owner parent was not made durable through its existing ancestor"
            );
            assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"keep");
        }
        assert!(!paths.state_dir.join("leaving").exists());
    }
}

#[tokio::test]
async fn a_sign_out_whose_owner_record_stays_after_the_revoke_finishes_when_run_again() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let private = root.join(".koidrive");
    let (server, served) = serve_calling(vec![access_token(), (204, Vec::new())], 1, {
        let private = private.clone();
        move || {
            close_dir(&private);
        }
    })
    .await;
    let paths = linked_with_server(&base, server);
    owned(&paths, &root);
    let driver = Driver::open(paths.clone()).unwrap();

    let failed = driver.sign_out(false).await;
    let revoked = served.await.unwrap();
    open_dir(&private);
    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block writes");
        return;
    }
    let failed = failed.unwrap().not_removed;
    assert_eq!(revoked, ["/v1/token", "/v1/devices/d1"]);
    let [failure] = failed.as_slice() else {
        panic!("one path stays: {failed:?}");
    };
    assert_eq!(failure.path, private.join("device.json"));
    assert!(
        failure.error.contains("ermission denied"),
        "{}",
        failure.error
    );
    assert_eq!(driver.status().account, None);
    assert!(!paths.credential().exists());
    assert!(driver.status().folders.is_empty());
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    drop(driver);

    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(driver.sign_out(false).await.unwrap().not_removed, []);
    assert_eq!(claim::owner(&root).unwrap(), None);
    assert!(driver.status().folders.is_empty());
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
}

#[tokio::test]
async fn a_sign_out_whose_store_stays_after_the_revoke_finishes_when_run_again_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let spaces = Paths::under(&base.join("profile")).spaces();
    let (server, served) = serve_calling(vec![access_token(), (204, Vec::new())], 1, {
        let spaces = spaces.clone();
        move || {
            close_dir(&spaces);
        }
    })
    .await;
    let paths = linked_with_server(&base, server);
    owned(&paths, &root);
    let store_dir = paths.store(SPACE).unwrap().parent().unwrap().to_path_buf();
    let driver = Driver::open(paths.clone()).unwrap();

    let failed = driver.sign_out(false).await;
    let revoked = served.await.unwrap();
    open_dir(&spaces);
    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block writes");
        return;
    }
    let failed = failed.unwrap().not_removed;
    assert_eq!(revoked, ["/v1/token", "/v1/devices/d1"]);
    let [failure] = failed.as_slice() else {
        panic!("one path stays: {failed:?}");
    };
    assert_eq!(failure.path, store_dir);
    assert_eq!(driver.status().account, None);
    assert!(!paths.credential().exists());
    assert_eq!(claim::owner(&root).unwrap(), None);
    drop(driver);

    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(driver.status().account, None);
    assert_eq!(driver.sign_out(false).await.unwrap().not_removed, []);
    assert!(driver.status().folders.is_empty());
    assert!(!store_dir.exists());
}

#[tokio::test]
async fn start_and_resume_cannot_replace_a_folder_task_during_sign_out() {
    use tokio::io::AsyncWriteExt;

    for held_at in [0, 1] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        let paths = linked_with_server(&base, server.clone());
        fs::write(root.join("file"), b"mine").unwrap();
        Store::open(paths.store(SPACE).unwrap())
            .unwrap()
            .transaction(|tx| {
                let mut link = tx.link()?.unwrap();
                link.server_url = server.clone();
                tx.set_link(&link)
            })
            .unwrap();
        claim::set_owner(
            &root,
            &Owner {
                server,
                device_id: "d1".into(),
            },
        )
        .unwrap();
        fs::write(paths.paused(), b"paused\n").unwrap();
        let (arrived, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let served = tokio::spawn(async move {
            let mut arrived = Some(arrived);
            let mut released = Some(released);
            for (i, (status, body)) in [access_token(), (204, Vec::new())].into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let _ = read_request(&mut socket).await;
                if i == held_at {
                    arrived.take().unwrap().send(()).unwrap();
                    released.take().unwrap().await.unwrap();
                }
                let head = format!(
                    "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        let driver = Driver::open(paths.clone()).unwrap();
        let signing_out = tokio::spawn({
            let driver = driver.clone();
            async move { driver.sign_out(false).await }
        });
        waiting.await.unwrap();
        driver.start();
        assert!(driver.resume().await.unwrap());
        let replacement = driver
            .inner
            .folders
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .run
            .as_ref()
            .map(|run| run.done.clone());
        release.send(()).unwrap();
        let left = signing_out.await.unwrap();
        served.await.unwrap();
        assert!(
            replacement.is_none(),
            "a task started while sign-out held the startup barrier"
        );
        let left = left.unwrap().not_removed;
        assert!(left.is_empty(), "{left:?}");
        signed_out(&driver, &paths, &root);
    }
}

#[tokio::test]
async fn a_failed_commit_and_credential_deletion_cannot_reexpose_a_revoked_account() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let state = Paths::under(&base.join("profile")).state_dir;
    let revoked = koidrive_proto::v1::Error {
        code: koidrive_proto::v1::error::Code::Unauthenticated.into(),
        message: "this device was revoked".into(),
        ..Default::default()
    };
    let (server, served) = serve_calling(
        vec![
            access_token(),
            (204, Vec::new()),
            (401, prost::Message::encode_to_vec(&revoked)),
        ],
        1,
        {
            let state = state.clone();
            move || {
                close_dir(&state);
                crate::leave::FAIL_PHASE.with(|failure| failure.set(Some(Phase::Committed)));
            }
        },
    )
    .await;
    let paths = linked_with_server(&base, server);
    owned(&paths, &root);
    let driver = Driver::open(paths.clone()).unwrap();
    let left = driver.sign_out(false).await;
    crate::leave::FAIL_PHASE.with(|failure| failure.set(None));
    open_dir(&state);
    if rustix::process::geteuid().is_root() {
        served.abort();
        let _ = served.await;
        return;
    }
    let left = left.unwrap().not_removed;
    assert!(
        left.iter()
            .any(|failure| failure.path == state.join("leaving"))
    );
    assert!(
        left.iter()
            .any(|failure| failure.path == paths.credential())
    );
    assert!(paths.credential().exists());
    assert!(matches!(
        Credential::load(&paths.credential()),
        Err(CredentialError::SignOutPending)
    ));
    assert_eq!(driver.status().account, None);
    drop(driver);

    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(driver.status().account, None);
    driver.start();
    assert!(driver.status().folders.is_empty());
    assert_eq!(driver.sign_out(false).await.unwrap().not_removed, []);
    assert_eq!(
        served.await.unwrap(),
        ["/v1/token", "/v1/devices/d1", "/v1/token"]
    );
    assert!(!paths.credential().exists());
    assert!(!state.join("leaving").exists());
}

#[tokio::test]
async fn signing_out_attempts_every_cleanup_and_stays_signed_out_when_the_credential_cannot_be_deleted()
 {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let other = base.join("Other");
    let paths = Paths::under(&base.join("profile"));
    let private = root.join(".koidrive");
    let store = paths.store(SPACE).unwrap().parent().unwrap().to_path_buf();
    let (server, served) = serve_calling(vec![access_token(), (204, Vec::new())], 1, {
        let private = private.clone();
        let held = store.join("held");
        let state = paths.state_dir.clone();
        move || {
            close_dir(&private);
            close_dir(&held);
            close_dir(&state);
        }
    })
    .await;
    let paths = linked_with_server(&base, server);
    owned(&paths, &root);
    let other_id = "0190f0a0-0000-7000-8000-000000000002";
    linked_to(&paths, &other, "d1", other_id);
    claim::set_owner(
        &other,
        &Owner {
            server: nowhere().into(),
            device_id: "d1".into(),
        },
    )
    .unwrap();
    fs::write(root.join("keep.txt"), b"keep").unwrap();
    fs::write(other.join("keep.txt"), b"keep too").unwrap();
    fs::create_dir(store.join("held")).unwrap();
    fs::write(store.join("held/f"), b"x").unwrap();
    let driver = Driver::open(paths.clone()).unwrap();

    let failed = driver.sign_out(false).await;
    assert_eq!(served.await.unwrap(), ["/v1/token", "/v1/devices/d1"]);
    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block writes");
        return;
    }
    let removing = paths.removing(SPACE, 1);
    open_dir(&private);
    open_dir(&removing.join("held"));
    open_dir(&paths.state_dir);
    let failed = failed.unwrap().not_removed;
    let mut remaining: Vec<_> = failed.iter().map(|failure| failure.path.clone()).collect();
    remaining.sort();
    let mut expected = vec![paths.credential(), private.join("device.json")];
    expected.sort();
    assert_eq!(remaining, expected, "{failed:?}");
    assert_eq!(driver.status().account, None);
    assert!(driver.status().folders.is_empty());
    assert!(paths.credential().exists());
    assert_eq!(claim::owner(&other).unwrap(), None);
    assert_eq!(Credential::load(&paths.credential()).unwrap(), None);
    assert!(
        Credential::load_stored(&paths.credential())
            .unwrap()
            .is_none()
    );
    assert!(!paths.store(other_id).unwrap().parent().unwrap().exists());
    assert!(!store.exists());
    assert!(removing.exists());
    drop(driver);

    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(driver.status().account, None);
    assert!(driver.status().folders.is_empty());
    assert_eq!(driver.sign_out(false).await.unwrap().not_removed, []);
    assert!(!paths.credential().exists());
    assert_eq!(claim::owner(&root).unwrap(), None);
    assert!(!store.exists());
    assert!(!removing.exists());
    assert!(!paths.state_dir.join("leaving").exists());
    assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"keep");
    assert_eq!(fs::read(other.join("keep.txt")).unwrap(), b"keep too");
}

#[test]
fn a_record_of_versions_an_unlink_brought_back_that_is_not_a_file_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(dir.path().join(UNLINK_BROUGHT_BACK))
        .status()
        .unwrap();
    assert!(made.success());

    assert_eq!(
        recorded_brought_back(&dir.path().join("store")),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn state_records_that_are_not_files_are_refused_without_waiting_on_them() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let root = dir.path().join("folder");
    let owner = root.join(koidrive_fs::PRIVATE_DIR).join("device.json");
    let records = [
        paths.config_file.clone(),
        paths.credential(),
        paths.state_dir.join("leaving"),
        owner,
    ];
    for record in &records {
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        koidrive_test_support::make_fifo(record);
    }

    let refused = koidrive_test_support::without_waiting(move || {
        [
            crate::config::Config::load(&paths.config_file).is_err(),
            crate::credential::Credential::load(&paths.credential()).is_err(),
            crate::leave::Leaving::load(&paths.state_dir).is_err(),
            claim::owner(&root).is_err(),
        ]
    });
    assert_eq!(refused, [true; 4]);
}

#[test]
fn a_record_of_an_unconfirmed_sign_out_that_is_not_a_file_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    fs::create_dir_all(paths.unconfirmed_revocation().parent().unwrap()).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(paths.unconfirmed_revocation())
        .status()
        .unwrap();
    assert!(made.success());

    assert_eq!(Unconfirmed::recorded(&paths), None);
}

#[tokio::test]
async fn a_confirmed_revocation_after_a_crash_left_an_unconfirmed_record_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let credential_path = Paths::under(&base.join("profile")).credential();
    let (server, served) =
        serve_each(vec![access_token(), (204, Vec::new())], credential_path).await;
    let paths = linked_with_server(&base, server.clone());
    fs::write(
        paths.unconfirmed_revocation(),
        serde_json::to_vec(&Unconfirmed {
            server: server.to_string(),
            device_id: "d1".into(),
            reason: "no answer within 10 seconds".into(),
        })
        .unwrap(),
    )
    .unwrap();
    let driver = Driver::open(paths.clone()).unwrap();

    let left = driver.sign_out(false).await.unwrap();
    let again = driver.sign_out(false).await.unwrap();

    let requested: Vec<String> = (served.await.unwrap().into_iter())
        .map(|(path, _)| path)
        .collect();
    assert_eq!(requested, ["/v1/token", "/v1/devices/d1"]);
    assert_eq!(left.unconfirmed, None);
    assert_eq!(again.unconfirmed, None);
}

#[tokio::test]
async fn a_device_that_is_not_the_last_revokes_itself_without_confirming() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let credential_path = Paths::under(&base.join("profile")).credential();
    let (server, served) =
        serve_each(vec![access_token(), (204, Vec::new())], credential_path).await;
    let paths = linked_with_server(&base, server);
    let driver = Driver::open(paths.clone()).unwrap();

    driver.sign_out(false).await.unwrap();
    assert_eq!(driver.status().account, None);
    assert!(!paths.credential().exists());
    let requested: Vec<String> = served
        .await
        .unwrap()
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert_eq!(requested, ["/v1/token", "/v1/devices/d1"]);
}
