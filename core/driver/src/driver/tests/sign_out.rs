// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn signing_out_unlinks_every_folder_and_forgets_the_credential() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    fs::write(root.join("keep.txt"), b"mine").unwrap();
    fs::create_dir_all(root.join(".koidrive/quarantine")).unwrap();
    fs::write(root.join(".koidrive/quarantine/e1"), b"held").unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;
    let mut watch = driver.watch();
    watch.borrow_and_update();

    let date = koidrive_fs::utc_date(SystemTime::now());
    let left = driver.sign_out(true).await.unwrap();
    if koidrive_fs::utc_date(SystemTime::now()) != date {
        eprintln!("SKIPPED: the UTC date changed during the test");
        return;
    }
    let held = root.join(format!("e1 (recovered {date})"));
    assert_eq!(left.set_aside.brought_back, std::slice::from_ref(&held));
    assert_eq!(fs::read(held).unwrap(), b"held");
    let status = driver.status();
    assert_eq!(status.account, None);
    assert!(!status.running);
    assert!(status.folders.is_empty());
    assert!(watch.has_changed().unwrap());
    assert!(!paths.credential().exists());
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    assert_eq!(claim::owner(&root).unwrap(), None);
    assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"mine");

    let (server, served) = serve_once(issued("d3")).await;
    driver.pair(&server, "pair-code", "laptop").await.unwrap();
    served.await.unwrap();
    drop(driver);
    let reopened = Driver::open(paths).unwrap().status();
    assert_eq!(reopened.account.unwrap().device_id, "d3");
    assert!(reopened.folders.is_empty());
}

#[tokio::test]
async fn saving_a_credential_unlinks_only_the_folders_of_a_lost_one() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    fs::write(root.join("notes.txt"), b"kept").unwrap();
    let driver = Driver::open(paths.clone()).unwrap();

    let refused = driver.save_credential(credential("d2")).await.unwrap_err();

    assert!(
        matches!(&refused, DriverError::AlreadyPaired { device_id } if device_id == "d1"),
        "{refused}"
    );
    assert_eq!(driver.status().folders.len(), 1);
    drop(driver);
    fs::remove_file(paths.credential()).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();

    let (account, _) = driver.save_credential(credential("d2")).await.unwrap();

    assert_eq!(account.device_id, "d2");
    assert!(driver.status().folders.is_empty());
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    assert_eq!(fs::read(root.join("notes.txt")).unwrap(), b"kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_saved_during_a_stale_sign_out_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    let (held, folders_held) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn({
        let driver = driver.clone();
        move || {
            let _folders = driver.inner.folders.lock().unwrap();
            held.send(()).unwrap();
            released.recv().unwrap();
        }
    });
    folders_held.recv().unwrap();
    let stale = tokio::spawn({
        let driver = driver.clone();
        async move { driver.sign_out_device("d1").await }
    });
    while driver.inner.running.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    assert!(driver.inner.lifecycle.try_lock().is_err());
    let save = tokio::spawn({
        let driver = driver.clone();
        async move { driver.save_credential(credential("d2")).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(!save.is_finished(), "the save waits for the sign-out");
    assert!(!paths.credential().exists());
    release.send(()).unwrap();
    holder.join().unwrap();
    stale.await.unwrap().unwrap();
    assert_eq!(save.await.unwrap().unwrap().0.device_id, "d2");
    assert!(paths.credential().exists());
    assert_eq!(driver.status().account.unwrap().device_id, "d2");
}

fn snapshot(dir: &Path) -> Vec<(PathBuf, SystemTime, Vec<u8>)> {
    let mut found = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(next) = todo.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let bytes = if meta.is_file() {
                fs::read(&path).unwrap()
            } else {
                todo.push(path.clone());
                Vec::new()
            };
            found.push((path, meta.modified().unwrap(), bytes));
        }
    }
    found.sort();
    found
}

#[test]
fn linked_folders_are_read_without_writing_to_the_profile() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    drop(Driver::open(paths.clone()).unwrap());
    let before = snapshot(&base.join("profile"));

    let folders = Driver::linked_folders(&paths).unwrap();

    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].space_id, SPACE);
    assert_eq!(folders[0].path, Some(root.display().to_string()));
    assert_eq!(snapshot(&base.join("profile")), before);

    let held = Driver::open(paths.clone()).unwrap();
    assert!(matches!(
        Driver::linked_folders(&paths),
        Err(DriverError::Busy(_))
    ));
    drop(held);

    let fresh = Paths::under(&base.join("fresh"));
    assert!(Driver::linked_folders(&fresh).unwrap().is_empty());
    assert!(!base.join("fresh").exists());
}

#[tokio::test]
async fn signing_out_a_refused_device_keeps_a_credential_saved_for_another() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d2");
    let driver = Driver::open(paths.clone()).unwrap();

    let error = driver.sign_out_device("d1").await.unwrap_err();

    assert!(
        matches!(&error, DriverError::CredentialReplaced { device_id } if device_id == "d2"),
        "{error}"
    );
    assert!(paths.credential().exists());
    let status = driver.status();
    assert_eq!(status.account.unwrap().device_id, "d2");
    assert_eq!(status.folders.len(), 1);
}

#[tokio::test]
async fn signing_out_without_the_server_waits_for_a_confirmation_and_then_finishes_on_the_device() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let driver = Driver::open(paths.clone()).unwrap();

    assert!(matches!(
        driver.sign_out(false).await,
        Err(DriverError::NotRevoked(_))
    ));
    assert_eq!(driver.status().account.unwrap().device_id, "d1");
    assert_eq!(driver.status().folders.len(), 1);
    assert!(paths.credential().exists());

    driver.sign_out(true).await.unwrap();
    assert_eq!(driver.status().account, None);
    assert!(driver.status().folders.is_empty());
    assert!(!paths.credential().exists());
}

async fn revocation_answered_with(raw: &'static [u8], hold: bool) -> String {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        write_reply(&mut socket, &access_token().1).await;
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket.write_all(raw).await.unwrap();
        if hold {
            std::future::pending::<()>().await;
        }
    });
    server
}

#[tokio::test]
async fn any_409_or_a_stalled_answer_keeps_the_credential_and_the_folders() {
    const EMPTY: &[u8] = b"HTTP/1.1 409 Conflict\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    const HTML: &[u8] = b"HTTP/1.1 409 Conflict\r\ncontent-type: text/html\r\ncontent-length: 13\r\nconnection: close\r\n\r\n<html></html>";
    const TRUNCATED: &[u8] = b"HTTP/1.1 409 Conflict\r\ncontent-type: application/x-protobuf\r\ncontent-length: 100\r\nconnection: close\r\n\r\n\x08\x10";
    // The status of a 409 whose body cannot be read may be lost with the body; either refusal keeps everything.
    let cases = [
        ("an empty 409", EMPTY, false, true),
        ("an HTML 409", HTML, false, true),
        ("a truncated 409", TRUNCATED, false, false),
        ("a stalled 409", TRUNCATED, true, false),
    ];
    for (case, raw, hold, status_kept) in cases {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let server = revocation_answered_with(raw, hold).await;
        let paths = linked_with_server(&base, server);
        let driver = Driver::open(paths.clone()).unwrap();

        let refused = driver.sign_out(false).await.unwrap_err();
        let expected = matches!(refused, DriverError::LastDevice)
            || !status_kept && matches!(refused, DriverError::NotRevoked(_));
        assert!(expected, "{case}: {refused:?}");
        assert!(paths.credential().exists(), "{case}");
        assert_eq!(driver.status().folders.len(), 1, "{case}");
    }
}

#[tokio::test]
async fn refused_signout_restores_only_previously_active_update_checks() {
    for response in [
        b"HTTP/1.1 409 Conflict\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".as_slice(),
        b"HTTP/1.1 500 Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".as_slice(),
    ] {
        for active in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(dir.path()).unwrap();
            let server = revocation_answered_with(response, false).await;
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
            let driver = release_driver(paths);
            if active {
                driver.check_for_updates();
            }
            tokio::task::yield_now().await;
            assert_eq!(driver.inner.updates.is_running(), active);
            let refused = driver.sign_out(false).await.unwrap_err();
            assert!(matches!(
                refused,
                DriverError::LastDevice | DriverError::NotRevoked(_)
            ));
            tokio::task::yield_now().await;
            assert_eq!(driver.inner.updates.is_running(), active, "{refused:?}");
            driver.stop().await;
            assert!(!driver.inner.updates.is_running());
        }
    }
}

#[tokio::test]
async fn a_confirmed_sign_out_that_gets_a_409_finishes_on_the_device() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let server = revocation_answered_with(
        b"HTTP/1.1 409 Conflict\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        false,
    )
    .await;
    let paths = linked_with_server(&base, server);
    let driver = Driver::open(paths.clone()).unwrap();

    driver.sign_out(true).await.unwrap();
    assert_eq!(driver.status().account, None);
    assert!(driver.status().folders.is_empty());
    assert!(!paths.credential().exists());
}

#[tokio::test]
async fn signing_out_waits_for_a_claim_in_flight_and_then_undoes_it() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let (server, held) = serve_held(vec![issued("d1")]).await;
    let claim = tokio::spawn({
        let driver = driver.clone();
        async move { driver.claim(&server, "code", "Ada", "laptop").await }
    });
    held.arrived.await.unwrap();
    let sign_out = tokio::spawn({
        let driver = driver.clone();
        async move { driver.sign_out(true).await }
    });
    assert!(still_waiting(&sign_out).await);

    held.release.send(()).unwrap();
    assert_eq!(claim.await.unwrap().unwrap().0.device_id, "d1");
    sign_out.await.unwrap().unwrap();
    assert_eq!(held.served.await.unwrap(), ["/v1/claim"]);
    assert_eq!(driver.status().account, None);
    assert!(!paths.credential().exists());
}

#[tokio::test]
async fn a_pair_during_a_claim_keeps_the_claimed_credential() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let (server, held) = serve_held(vec![issued("d1")]).await;
    let claim = tokio::spawn({
        let driver = driver.clone();
        async move { driver.claim(&server, "code", "Ada", "laptop").await }
    });
    held.arrived.await.unwrap();
    let (pair_server, pair_served) = serve_once(issued("d2")).await;
    let pair = tokio::spawn({
        let driver = driver.clone();
        async move { driver.pair(&pair_server, "code", "desktop").await }
    });
    assert!(still_waiting(&pair).await);

    held.release.send(()).unwrap();
    assert_eq!(claim.await.unwrap().unwrap().0.device_id, "d1");
    assert!(matches!(
        pair.await.unwrap(),
        Err(DriverError::AlreadyPaired { device_id }) if device_id == "d1"
    ));
    assert!(!pair_served.is_finished(), "the pair reached the server");
    pair_served.abort();
    held.served.await.unwrap();
    drop(driver);
    let reopened = Driver::open(paths).unwrap().status();
    assert_eq!(reopened.account.unwrap().device_id, "d1");
}

#[tokio::test]
async fn signing_out_waits_for_a_link_in_flight_and_then_unlinks_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    fs::create_dir(&root).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let token = prost::Message::encode_to_vec(&koidrive_proto::v1::AccessToken {
        token: "token".into(),
        expires_at_ms: i64::MAX,
        ..Default::default()
    });
    let spaces = prost::Message::encode_to_vec(&koidrive_proto::v1::SpaceList {
        spaces: vec![SpaceInfo {
            space_id: SPACE.into(),
            name: "Projects".into(),
            ..Default::default()
        }],
        ..Default::default()
    });
    let (server, held) = serve_held(vec![token, spaces]).await;
    Credential {
        server,
        ..credential("d1")
    }
    .save_new(&paths.credential())
    .unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    let link = tokio::spawn({
        let driver = driver.clone();
        let root = root.clone();
        async move { driver.link(&root, "Projects").await }
    });
    held.arrived.await.unwrap();
    let sign_out = tokio::spawn({
        let driver = driver.clone();
        async move { driver.sign_out(true).await }
    });
    assert!(still_waiting(&sign_out).await);

    held.release.send(()).unwrap();
    link.await.unwrap().unwrap();
    sign_out.await.unwrap().unwrap();
    assert_eq!(held.served.await.unwrap(), ["/v1/token", "/v1/spaces"]);
    let status = driver.status();
    assert_eq!(status.account, None);
    assert!(status.folders.is_empty());
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    assert_eq!(claim::owner(&root).unwrap(), None);
    assert!(!paths.credential().exists());
}

#[tokio::test]
async fn a_folder_whose_owner_record_stays_keeps_the_device_signed_in_until_a_retry_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    claim::set_owner(
        &root,
        &Owner {
            server: nowhere().into(),
            device_id: "d1".into(),
        },
    )
    .unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    let mut watch = driver.watch();
    let private = root.join(koidrive_fs::PRIVATE_DIR);
    if !close_dir(&private) {
        return;
    }

    let failed = driver.sign_out(true).await;
    open_dir(&private);
    assert!(
        matches!(failed, Err(DriverError::UnlinkBlocked { .. })),
        "{failed:?}"
    );
    watch.borrow_and_update();
    let status = driver.status();
    assert_eq!(status.account.unwrap().device_id, "d1");
    assert_eq!(status.folders[0].problem, None);
    assert!(paths.credential().exists());
    assert!(paths.store(SPACE).unwrap().exists());
    assert!(claim::owner(&root).unwrap().is_some());

    driver.sign_out(true).await.unwrap();
    assert!(watch.has_changed().unwrap());
    assert!(driver.status().folders.is_empty());
    assert_eq!(claim::owner(&root).unwrap(), None);
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    assert!(!paths.credential().exists());
}

#[tokio::test]
async fn a_failed_unlink_wakes_watchers_and_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let driver = Driver::open(paths.clone()).unwrap();
    let mut watch = driver.watch();
    let store_dir = paths.store(SPACE).unwrap().parent().unwrap().to_path_buf();
    if !close_dir(&store_dir) {
        return;
    }

    let failed = driver.unlink(&root).await;
    open_dir(&store_dir);
    assert!(matches!(failed, Err(DriverError::Io { .. })), "{failed:?}");
    assert!(watch.has_changed().unwrap());
    watch.borrow_and_update();
    assert_eq!(driver.status().folders[0].state, FolderState::Halted);
    assert!(paths.store(SPACE).unwrap().exists());

    driver.unlink(&root).await.unwrap();
    assert!(watch.has_changed().unwrap());
    assert!(driver.status().folders.is_empty());
    assert!(!store_dir.exists());
}

struct Gate(Arc<(Mutex<bool>, std::sync::Condvar)>);

impl Gate {
    fn open(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.open();
    }
}

struct Paused {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl Matcher for Paused {
    fn is_excluded(&self, _: &Path, _: bool) -> bool {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        let (open, opened) = &*self.gate;
        let _open = opened
            .wait_while(open.lock().unwrap(), |open| !*open)
            .unwrap();
        false
    }
}
