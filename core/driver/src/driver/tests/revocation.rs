// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn review_cancel_after_remote_revoke_does_not_reload_the_old_account() {
    use tokio::io::AsyncWriteExt;
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let private = root.join(".koidrive");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let paths = linked_with_server(&base, server);
    owned(&paths, &root);
    let (revoked, accepted) = tokio::sync::oneshot::channel();
    let served = tokio::spawn({
        let private = private.clone();
        async move {
            for (status, body) in [access_token(), (204, Vec::new())] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_request(&mut socket).await.0;
                let head = format!(
                    "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
                if request == "/v1/devices/d1" {
                    close_dir(&private);
                    revoked.send(()).unwrap();
                    return;
                }
            }
        }
    });
    let driver = Driver::open(paths.clone()).unwrap();
    {
        let leave = driver.sign_out(false);
        tokio::pin!(leave);
        tokio::select! {
            biased;
            _ = accepted => {},
            result = &mut leave => panic!("sign-out completed before cancellation: {result:?}"),
        }
    }
    served.await.unwrap();
    drop(driver);
    let reopened = Driver::open(paths.clone()).unwrap();
    let account = reopened.status().account;
    let retry = reopened.sign_out(false).await;
    open_dir(&private);
    assert!(
        account.is_none(),
        "the remotely revoked account became locally signed in again"
    );
    assert!(matches!(retry, Err(DriverError::NotRevoked(_))));
}

async fn recovery_refusal_stays_blocked(status: u16) {
    for reopen in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let credential_path = Paths::under(&base.join("profile")).credential();
        let unauthenticated = (
            401,
            prost::Message::encode_to_vec(&koidrive_proto::v1::Error {
                code: koidrive_proto::v1::error::Code::Unauthenticated.into(),
                message: "the earlier DELETE revoked this device".into(),
                ..Default::default()
            }),
        );
        let (server, served) = serve_each(
            vec![
                access_token(),
                (500, Vec::new()),
                (status, Vec::new()),
                unauthenticated,
            ],
            credential_path,
        )
        .await;
        let paths = linked_with_server(&base, server);
        let mut driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        assert!(matches!(
            driver.sign_out(false).await,
            Err(DriverError::NotRevoked(_))
        ));
        let credential = fs::read(paths.credential()).unwrap();
        if reopen {
            drop(driver);
            driver = Driver::open(paths.clone()).unwrap();
        }
        assert!(driver.sign_out(false).await.is_err());
        let account = driver.status().account;
        let running = driver.status().running;
        let pending = matches!(
            Credential::load(&paths.credential()),
            Err(CredentialError::SignOutPending)
        );
        let revoking = matches!(
            crate::leave::phase(&paths.state_dir),
            Ok(Some(Phase::Revoking))
        );
        let raw_kept = fs::read(paths.credential()).unwrap() == credential;
        let store_kept = paths.store(SPACE).unwrap().exists();
        driver.sign_out(false).await.unwrap();
        assert_eq!(served.await.unwrap().len(), 4);
        assert!(
            account.is_none(),
            "recovery {status} restored the account, reopen={reopen}"
        );
        assert!(!running, "recovery {status} resumed sync, reopen={reopen}");
        assert!(pending && revoking && raw_kept && store_kept);
        assert!(!paths.credential().exists());
        assert!(driver.status().folders.is_empty());
    }
}

#[tokio::test]
async fn a_recovery_429_cannot_restore_the_account() {
    recovery_refusal_stays_blocked(429).await;
}

#[tokio::test]
async fn a_recovery_409_restores_the_account() {
    for reopen in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let credential_path = Paths::under(&base.join("profile")).credential();
        let (server, served) = serve_each(
            vec![access_token(), (500, Vec::new()), last_device_refused()],
            credential_path,
        )
        .await;
        let paths = linked_with_server(&base, server);
        let mut driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        assert!(matches!(
            driver.sign_out(false).await,
            Err(DriverError::NotRevoked(_))
        ));
        if reopen {
            drop(driver);
            driver = Driver::open(paths.clone()).unwrap();
        }
        assert!(matches!(
            driver.sign_out(false).await,
            Err(DriverError::LastDevice)
        ));
        assert_eq!(served.await.unwrap().len(), 3);
        assert!(
            driver.status().account.is_some(),
            "recovery 409 left the account signed out, reopen={reopen}"
        );
        assert!(
            driver.status().running,
            "recovery 409 left sync stopped, reopen={reopen}"
        );
        assert!(matches!(crate::leave::phase(&paths.state_dir), Ok(None)));
        assert!(Credential::load(&paths.credential()).is_ok());
        assert!(paths.store(SPACE).unwrap().exists());
    }
}

async fn prepared_retry_restores_authority(running: bool, checking: bool) {
    use tokio::io::AsyncWriteExt;

    for (reopen, updates_off) in [(false, false), (true, false), (true, true)] {
        if updates_off && !checking {
            continue;
        }
        for paused in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(dir.path()).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let server = format!("http://{}/", listener.local_addr().unwrap());
            let paths = linked_with_server(&base, server);
            fs::write(
                paths.update_check(),
                serde_json::json!({
                    "asset": crate::update::this_asset(),
                    "checked_at": i64::MAX - 1000,
                    "successful_at": 100,
                })
                .to_string(),
            )
            .unwrap();
            let (entered, token_started) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let served = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                assert_eq!(read_request(&mut socket).await.0, "/v1/token");
                entered.send(()).unwrap();
                released.await.unwrap();
                drop(socket);
                let mut calls = Vec::new();
                for (status, body) in [access_token(), last_device_refused()] {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    calls.push(read_request(&mut socket).await.0);
                    let head = format!(
                        "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                }
                calls
            });
            let mut driver = release_driver(paths.clone());
            if running {
                driver.start();
            }
            if checking {
                driver.check_for_updates();
            }
            let before = driver.status().folders;
            {
                let leave = driver.sign_out(false);
                tokio::pin!(leave);
                tokio::select! {
                    biased;
                    _ = token_started => {},
                    result = &mut leave => panic!("sign-out finished before Prepared cancellation: {result:?}"),
                }
            }
            assert!(matches!(
                crate::leave::phase(&paths.state_dir),
                Ok(Some(Phase::Prepared))
            ));
            assert!(!driver.status().running);
            assert!(!driver.inner.updates.is_running());
            if paused {
                driver.pause().await.unwrap();
                fs::write(paths.paused(), b"pause stays\n").unwrap();
            }
            if updates_off {
                fs::write(&paths.config_file, "[updates]\ncheck = false\n").unwrap();
            }
            if reopen {
                drop(driver);
                driver = release_driver(paths.clone());
            }
            release.send(()).unwrap();
            assert!(matches!(
                driver.sign_out(false).await,
                Err(DriverError::LastDevice)
            ));
            assert_eq!(served.await.unwrap(), ["/v1/token", "/v1/devices/d1"]);
            let status = driver.status();
            let checking_after = driver.inner.updates.is_running();
            driver.stop().await;
            assert_eq!(
                status.running, running,
                "running authority lost, reopen={reopen}, paused={paused}"
            );
            assert_eq!(
                checking_after,
                checking && !updates_off,
                "update authority changed, reopen={reopen}, paused={paused}, updates_off={updates_off}"
            );
            if updates_off {
                assert_eq!(status.update, crate::Update::Off);
                assert!(!paths.update_check().exists());
            }
            assert_eq!(status.paused, paused);
            if paused {
                assert_eq!(fs::read(paths.paused()).unwrap(), b"pause stays\n");
                assert_eq!(status.folders.len(), before.len());
                for (folder, before) in status.folders.iter().zip(&before) {
                    assert_eq!(folder.space_id, before.space_id);
                    if before.state == FolderState::Halted {
                        assert_eq!(folder.state, FolderState::Halted);
                        assert_eq!(folder.problem, before.problem);
                    } else {
                        assert_eq!(folder.state, FolderState::Stopped);
                    }
                }
            } else {
                assert!(!paths.paused().exists());
            }
            assert!(status.account.is_some());
            assert!(!paths.state_dir.join("leaving").exists());
        }
    }
}

#[tokio::test]
async fn prepared_retry_preserves_running_authority() {
    prepared_retry_restores_authority(true, false).await;
}

#[tokio::test]
async fn prepared_retry_preserves_update_check_authority() {
    prepared_retry_restores_authority(false, true).await;
}

#[tokio::test]
async fn prepared_retry_does_not_invent_running_or_update_check_authority() {
    prepared_retry_restores_authority(false, false).await;
}

#[tokio::test]
async fn an_interrupted_revocation_resolves_before_restoring_the_account_or_checking_cleanup() {
    use koidrive_net::CredentialStore;
    use tokio::io::AsyncWriteExt;

    for remote_revoked in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let private = root.join(".koidrive");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        let paths = linked_with_server(&base, server);
        owned(&paths, &root);
        fs::write(paths.paused(), b"paused\n").unwrap();
        let (sent, accepted) = tokio::sync::oneshot::channel();
        let mut replies = vec![access_token()];
        let unauthenticated = (
            401,
            prost::Message::encode_to_vec(&koidrive_proto::v1::Error {
                code: koidrive_proto::v1::error::Code::Unauthenticated.into(),
                message: "this device was revoked".into(),
                ..Default::default()
            }),
        );
        if remote_revoked {
            replies.push((204, Vec::new()));
            replies.push(unauthenticated);
        } else {
            replies.extend([
                last_device_refused(),
                access_token(),
                last_device_refused(),
                unauthenticated,
            ]);
        }
        let served = tokio::spawn({
            let private = private.clone();
            let state = paths.state_dir.clone();
            async move {
                let mut sent = Some(sent);
                let mut calls = Vec::new();
                for (i, (status, body)) in replies.into_iter().enumerate() {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    calls.push(read_request(&mut socket).await.0);
                    let head = format!(
                        "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                    if i == 1 {
                        assert!(matches!(
                            crate::leave::phase(&state),
                            Ok(Some(Phase::Revoking))
                        ));
                        close_dir(&private);
                        sent.take().unwrap().send(()).unwrap();
                    }
                }
                calls
            }
        });
        let driver = Driver::open(paths.clone()).unwrap();
        let credential = Credential::load(&paths.credential()).unwrap().unwrap();
        driver.start();
        {
            let leave = driver.sign_out(false);
            tokio::pin!(leave);
            tokio::select! {
                biased;
                _ = accepted => {},
                result = &mut leave => panic!("sign-out completed before interruption: {result:?}"),
            }
        }
        assert_eq!(driver.status().account, None);
        drop(driver);

        let driver = Driver::open(paths.clone()).unwrap();
        assert_eq!(driver.status().account, None);
        assert!(!driver.status().running);
        assert!(driver.status().paused);
        assert!(matches!(
            Credential::load(&paths.credential()),
            Err(CredentialError::SignOutPending)
        ));
        assert!(matches!(
            Credential::load_stored(&paths.credential()),
            Err(CredentialError::SignOutPending)
        ));
        for refused in [
            driver.pair(nowhere(), "code", "other").await,
            driver.claim(nowhere(), "code", "Ada", "other").await,
            driver.save_credential(credential.clone()).await,
        ] {
            assert!(matches!(
                refused,
                Err(DriverError::Credential(CredentialError::SignOutPending))
            ));
        }
        let bytes = fs::read(paths.credential()).unwrap();
        let files = CredentialFiles::new(&paths.credential(), credential, Arc::default());
        let staged = koidrive_net::Staged {
            refresh_credential: "replacement".into(),
            rotate_before_ms: i64::MAX,
        };
        assert!(files.stage(&staged).is_err());
        assert!(files.promote(&staged).is_err());
        assert_eq!(fs::read(paths.credential()).unwrap(), bytes);
        assert!(matches!(
            crate::leave::phase(&paths.state_dir),
            Ok(Some(Phase::Revoking))
        ));

        let retry = driver.sign_out(false).await;
        let failed = if remote_revoked {
            retry.unwrap().not_removed
        } else {
            assert!(matches!(retry, Err(DriverError::LastDevice)));
            assert!(driver.status().account.is_some());
            assert!(driver.status().paused);
            assert!(matches!(crate::leave::phase(&paths.state_dir), Ok(None)));
            assert!(Credential::load(&paths.credential()).unwrap().is_some());
            assert!(paths.store(SPACE).unwrap().exists());
            open_dir(&private);
            assert_eq!(driver.sign_out(false).await.unwrap().not_removed, []);
            Vec::new()
        };
        let calls = served.await.unwrap();
        open_dir(&private);
        if remote_revoked {
            assert_eq!(calls, ["/v1/token", "/v1/devices/d1", "/v1/token"]);
        } else {
            assert_eq!(
                calls,
                [
                    "/v1/token",
                    "/v1/devices/d1",
                    "/v1/token",
                    "/v1/devices/d1",
                    "/v1/token"
                ]
            );
        }
        assert_eq!(driver.status().account, None);
        assert!(!paths.credential().exists());
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
        if remote_revoked && !rustix::process::geteuid().is_root() {
            assert_eq!(failed.len(), 1);
            assert_eq!(failed[0].path, private.join("device.json"));
        }
        drop(driver);
        let driver = Driver::open(paths.clone()).unwrap();
        assert_eq!(driver.sign_out(false).await.unwrap().not_removed, []);
        assert_eq!(claim::owner(&root).unwrap(), None);
    }
}
