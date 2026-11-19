// SPDX-License-Identifier: Apache-2.0

#[cfg(target_os = "macos")]
#[test]
fn opening_the_profile_excludes_its_credential_from_time_machine_again() {
    use crate::credential::BACKUP_EXCLUSION;
    use crate::credential::tests::time_machine_excludes;

    let home = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    let paths = Paths::under(home.path());
    paired(&paths, "d1");
    assert!(time_machine_excludes(&paths.credential()));
    rustix::fs::removexattr(paths.credential(), BACKUP_EXCLUSION.0).unwrap();
    assert!(!time_machine_excludes(&paths.credential()));

    drop(Driver::open(paths.clone()).unwrap());
    assert!(time_machine_excludes(&paths.credential()));
}

#[cfg(target_os = "macos")]
#[test]
fn opening_refuses_a_credential_it_cannot_keep_out_of_backups() {
    use crate::credential::tests::time_machine_excludes;
    use crate::credential::{BACKUP_EXCLUSION, CredentialError};

    let home = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    let paths = Paths::under(home.path());
    paired(&paths, "d1");
    rustix::fs::removexattr(paths.credential(), BACKUP_EXCLUSION.0).unwrap();
    struct Unlock(std::path::PathBuf);
    impl Drop for Unlock {
        fn drop(&mut self) {
            let status = std::process::Command::new("chflags")
                .arg("nouchg")
                .arg(&self.0)
                .status();
            if !std::thread::panicking() {
                assert!(status.unwrap().success());
            }
        }
    }
    let status = std::process::Command::new("chflags")
        .arg("uchg")
        .arg(paths.credential())
        .status()
        .unwrap();
    assert!(status.success());
    let unlock = Unlock(paths.credential());
    let refused = Driver::open(paths.clone()).err();
    drop(unlock);
    let error = refused.expect("the driver opened with a credential that backups would copy");
    assert!(
        matches!(
            error,
            DriverError::Credential(CredentialError::NotExcludedFromBackups { .. })
        ),
        "{error}"
    );
    assert!(error.to_string().contains("Time Machine"), "{error}");
    assert!(error.to_string().contains("restart Koidrive"), "{error}");

    drop(Driver::open(paths.clone()).unwrap());
    assert!(time_machine_excludes(&paths.credential()));
}

#[tokio::test]
async fn a_credential_that_backups_would_copy_is_refused_before_the_server_uses_up_the_code() {
    use crate::credential::tests::REFUSE_EXCLUSION;

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let (server, served) = serve_once(issued("d1")).await;
    REFUSE_EXCLUSION.set(true);
    let claimed = driver.claim(&server, "claim-code", "Ada", "laptop").await;
    let paired = driver.pair(&server, "pair-code", "laptop").await;
    REFUSE_EXCLUSION.set(false);
    for refused in [claimed.err(), paired.err()] {
        assert!(
            matches!(
                refused,
                Some(DriverError::Credential(
                    CredentialError::NotExcludedFromBackups { .. }
                ))
            ),
            "{refused:?}"
        );
    }
    let state = fs::read_dir(paths.credential().parent().unwrap()).unwrap();
    let left: Vec<_> = state.map(|entry| entry.unwrap().file_name()).collect();
    assert!(
        !left
            .iter()
            .any(|name| name.to_string_lossy().contains("credential")),
        "{left:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), served)
            .await
            .is_err(),
        "the server was asked for a credential"
    );
}

async fn serve_recorded(
    replies: Vec<(u16, Vec<u8>)>,
) -> (String, tokio::task::JoinHandle<Vec<(String, Vec<u8>)>>) {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            let head = format!(
                "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        }
        requests
    });
    (server, served)
}

fn rotation_answer(server_time_ms: i64, rotated: bool) -> (u16, Vec<u8>) {
    let token = koidrive_proto::v1::AccessToken {
        token: "token".into(),
        expires_at_ms: i64::MAX,
        server_time_ms,
        rotated,
    };
    (200, prost::Message::encode_to_vec(&token))
}

fn token_request(body: &[u8]) -> koidrive_proto::v1::TokenRequest {
    prost::Message::decode(body).unwrap()
}

#[tokio::test]
async fn a_rotation_reaches_linking_and_signing_out_through_the_one_auth() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    fs::create_dir(&root).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let spaces = prost::Message::encode_to_vec(&koidrive_proto::v1::SpaceList {
        spaces: vec![SpaceInfo {
            space_id: SPACE.into(),
            name: "Projects".into(),
            ..Default::default()
        }],
        ..Default::default()
    });
    let (server, served) = serve_recorded(vec![
        rotation_answer(5_000, false),
        rotation_answer(5_000, true),
        (200, spaces),
        (204, Vec::new()),
    ])
    .await;
    Credential {
        server,
        ..credential("d1")
    }
    .save_new(&paths.credential())
    .unwrap();
    let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
    fs::OpenOptions::new()
        .write(true)
        .open(paths.credential())
        .unwrap()
        .set_modified(two_days_ago)
        .unwrap();
    let driver = Driver::open(paths.clone()).unwrap();

    driver.link(&root, "Projects").await.unwrap();
    let rotated = Credential::load(&paths.credential())
        .unwrap()
        .unwrap()
        .refresh_credential;
    assert_ne!(rotated, "secret");
    assert!(!next_path(&paths.credential()).exists());
    driver.sign_out(true).await.unwrap();

    let requests = served.await.unwrap();
    let paths_requested: Vec<_> = requests.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(
        paths_requested[..3],
        ["/v1/token", "/v1/token", "/v1/spaces"]
    );
    assert!(
        paths_requested[3].starts_with("/v1/devices/d1"),
        "{paths_requested:?}"
    );
    let proposal = token_request(&requests[1].1);
    assert_eq!(proposal.refresh_credential, "secret");
    assert_eq!(proposal.next_refresh_credential, rotated);
    assert_eq!(proposal.rotate_before_ms, 5_000 + 60_000);
}

#[tokio::test]
async fn a_crash_between_the_two_writes_of_a_promotion_ends_with_one_file() {
    for stored_again in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let answer = if stored_again {
            rotation_answer(500, true)
        } else {
            rotation_answer(2_000, false)
        };
        let (server, served) = serve_recorded(vec![answer]).await;
        let promoted = Credential {
            server,
            refresh_credential: "promoted".into(),
            ..credential("d1")
        };
        promoted.save_new(&paths.credential()).unwrap();
        let staged = koidrive_net::Staged {
            refresh_credential: "promoted".into(),
            rotate_before_ms: 1_000,
        };
        koidrive_net::CredentialStore::stage(
            &CredentialFiles::new(&paths.credential(), promoted.clone(), Arc::default()),
            &staged,
        )
        .unwrap();
        let driver = Driver::open(paths.clone()).unwrap();

        driver
            .auth(&promoted)
            .unwrap()
            .access_token()
            .await
            .unwrap();
        let request = token_request(&served.await.unwrap()[0].1);
        assert_eq!(request.refresh_credential, "promoted");
        assert_eq!(request.next_refresh_credential, "promoted");
        assert!(!next_path(&paths.credential()).exists(), "{stored_again}");
        let stored = Credential::load_stored(&paths.credential())
            .unwrap()
            .unwrap();
        assert_eq!(stored.credential.refresh_credential, "promoted");
        assert_eq!(stored.staged, None);
    }
}

#[tokio::test]
async fn an_auth_built_after_open_starts_from_the_stored_slots() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let (server, served) = serve_recorded(vec![rotation_answer(500, true)]).await;
    let current = Credential {
        server,
        refresh_credential: "current".into(),
        ..credential("d1")
    };
    current.save_new(&paths.credential()).unwrap();
    koidrive_net::CredentialStore::stage(
        &CredentialFiles::new(&paths.credential(), current.clone(), Arc::default()),
        &koidrive_net::Staged {
            refresh_credential: "staged".into(),
            rotate_before_ms: 1_000,
        },
    )
    .unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    *driver.inner.auth.lock().unwrap() = None;

    driver.auth(&current).unwrap().access_token().await.unwrap();
    let request = token_request(&served.await.unwrap()[0].1);
    assert_eq!(request.refresh_credential, "current");
    assert_eq!(request.next_refresh_credential, "staged");
    let stored = Credential::load_stored(&paths.credential())
        .unwrap()
        .unwrap();
    assert_eq!(stored.credential.refresh_credential, "staged");
    assert_eq!(stored.staged, None);
}

#[tokio::test]
async fn signing_out_removes_the_staged_credential_too() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    paired(&paths, "d1");
    let stored = Credential::load(&paths.credential()).unwrap().unwrap();
    koidrive_net::CredentialStore::stage(
        &CredentialFiles::new(&paths.credential(), stored, Arc::default()),
        &koidrive_net::Staged {
            refresh_credential: "next".into(),
            rotate_before_ms: 1,
        },
    )
    .unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    driver.sign_out(true).await.unwrap();
    assert!(!paths.credential().exists());
    assert!(!next_path(&paths.credential()).exists());
}

#[tokio::test]
async fn linking_records_whether_the_folder_held_the_spaces_marker_already() {
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
    let (server, _served) =
        serve_recorded(vec![(200, token), (200, spaces.clone()), (200, spaces)]).await;
    Credential {
        server,
        ..credential("d1")
    }
    .save_new(&paths.credential())
    .unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    let found_marker = || {
        Store::open(paths.store(SPACE).unwrap())
            .unwrap()
            .transaction(|tx| tx.link())
            .unwrap()
            .unwrap()
            .found_marker
    };

    driver.link(&root, "Projects").await.unwrap();
    assert!(!found_marker());
    driver.unlink(&root).await.unwrap();
    assert_eq!(read_marker(&root).unwrap().unwrap().space_id, SPACE);
    driver.link(&root, "Projects").await.unwrap();
    assert!(found_marker());
    let store = paths.store(SPACE).unwrap();
    let link = (Store::open(&store).unwrap())
        .transaction(|tx| tx.link())
        .unwrap();
    assert_eq!(crate::link_copy::read(&store, SPACE), link);
}
