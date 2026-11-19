// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn a_claim_keeps_the_user_name_returned_by_the_server_on_retry() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let reply = prost::Message::encode_to_vec(&DeviceCredentials {
        user_id: "u1".into(),
        device_id: "d1".into(),
        refresh_credential: "secret".into(),
        user_name: "Original name".into(),
        ..Default::default()
    });
    let (server, served) = serve_once(reply).await;
    let (account, _) = driver
        .claim(&server, "claim-code", "Retried name", "laptop")
        .await
        .unwrap();
    assert_eq!(account.user_name.as_deref(), Some("Original name"));
    served.await.unwrap();
    drop(driver);
    assert_eq!(
        Driver::open(paths)
            .unwrap()
            .status()
            .account
            .unwrap()
            .user_name
            .as_deref(),
        Some("Original name")
    );
}

#[tokio::test]
async fn an_interrupted_claim_is_sent_again_with_its_request_id_and_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let url = url::Url::parse(&server).unwrap();
    let (arrived, first) = tokio::sync::oneshot::channel();
    let served = tokio::spawn(async move {
        let (mut held, _) = listener.accept().await.unwrap();
        let lost = read_request(&mut held).await.1;
        arrived.send(()).unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        let resent = read_request(&mut socket).await.1;
        write_reply(&mut socket, &issued("d1")).await;
        drop(held);
        (lost, resent)
    });

    tokio::select! {
        outcome = driver.claim(&server, "claim-code", "Ada", "laptop") => panic!("the claim ended: {outcome:?}"),
        _ = first => {}
    }
    assert!(crate::claim_pending(&paths, &url));
    let (account, _) = driver
        .claim(&server, "claim-code", "Ada", "laptop")
        .await
        .unwrap();

    assert_eq!(account.device_id, "d1");
    let (lost, resent) = served.await.unwrap();
    let lost: ClaimRequest = prost::Message::decode(lost.as_slice()).unwrap();
    let resent: ClaimRequest = prost::Message::decode(resent.as_slice()).unwrap();
    assert!(!lost.request_id.is_empty());
    assert_eq!(resent.request_id, lost.request_id);
    assert!(!crate::claim_pending(&paths, &url));
    assert_eq!(crate::pending_create::saved(&paths), 0);
}

fn request_ids<R: prost::Message + Default>(
    bodies: &[Vec<u8>],
    id: impl Fn(R) -> String,
) -> Vec<String> {
    (bodies.iter())
        .map(|body| id(R::decode(body.as_slice()).unwrap()))
        .collect()
}

fn code_sha256(normalized: &str) -> String {
    use sha2::Digest;
    (sha2::Sha256::digest(normalized).iter())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test]
async fn a_claim_the_previous_release_left_pending_is_sent_with_its_request_id() {
    use crate::pending_create::fake::{Answer, serve};

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let (server, served) = serve(vec![Answer::Reply(issued("d1"))]).await;
    let url = url::Url::parse(&server).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let left = serde_json::json!([
        { "server": "https://other.example/", "code_sha256": code_sha256("CLAIMCODE"), "request_id": "other-server" },
        { "server": url.as_str(), "code_sha256": code_sha256("OTHERCODE"), "request_id": "other-code" },
        { "server": url.as_str(), "code_sha256": code_sha256("CLAIMCODE"), "request_id": "alpha7-request-id" },
    ]);
    std::fs::write(paths.pending_claim(), left.to_string()).unwrap();
    std::fs::write(paths.state_dir.join(".pending-claim.json.42.tmp"), b"[]").unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(crate::claim_pending(&paths, &url));

    driver
        .claim(&server, "claim-code", "Ada", "laptop")
        .await
        .unwrap();

    let ids = request_ids(&served.await.unwrap(), |r: ClaimRequest| r.request_id);
    assert_eq!(ids, ["alpha7-request-id"]);
    assert!(!paths.pending_claim().exists());
    assert!(!paths.state_dir.join(".pending-claim.json.42.tmp").exists());
    assert!(!crate::claim_pending(&paths, &url));
    assert_eq!(crate::pending_create::saved(&paths), 0);
}

#[tokio::test]
async fn a_claim_the_previous_release_left_pending_is_forgotten_once_it_is_refused_for_good() {
    use crate::pending_create::fake::{Answer, serve};
    use koidrive_proto::v1::{Error as ProtoError, error::Code};

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let owned = ProtoError {
        code: Code::Forbidden as i32,
        message: "this server already has an owner".into(),
        ..ProtoError::default()
    };
    let (server, served) = serve(vec![Answer::Refuse(403, owned)]).await;
    let url = url::Url::parse(&server).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let left = serde_json::json!([
        { "server": url.as_str(), "code_sha256": code_sha256("CLAIMCODE"), "request_id": "alpha7-request-id" },
    ]);
    std::fs::write(paths.pending_claim(), left.to_string()).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();

    let refused = driver.claim(&server, "claim-code", "Ada", "laptop").await;

    assert!(matches!(refused, Err(DriverError::Api(_))), "{refused:?}");
    let ids = request_ids(&served.await.unwrap(), |r: ClaimRequest| r.request_id);
    assert_eq!(ids, ["alpha7-request-id"]);
    assert!(!crate::claim_pending(&paths, &url));
    assert!(!paths.pending_claim().exists());
}

#[tokio::test]
async fn a_claim_with_another_code_than_the_previous_release_left_draws_its_own_request_id() {
    use crate::pending_create::fake::{Answer, serve};

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let (server, served) = serve(vec![Answer::Reply(issued("d1"))]).await;
    let url = url::Url::parse(&server).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let left = serde_json::json!([
        { "server": url.as_str(), "code_sha256": code_sha256("OTHERCODE"), "request_id": "other-code" },
    ]);
    std::fs::write(paths.pending_claim(), left.to_string()).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();

    driver
        .claim(&server, "claim-code", "Ada", "laptop")
        .await
        .unwrap();

    let ids = request_ids(&served.await.unwrap(), |r: ClaimRequest| r.request_id);
    assert_eq!(ids.len(), 1);
    assert_ne!(ids[0], "other-code");
    assert!(!paths.pending_claim().exists());
}

#[tokio::test]
async fn an_unreadable_claim_record_of_the_previous_release_never_fails_a_claim() {
    use crate::pending_create::fake::{Answer, serve};

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let (server, served) = serve(vec![Answer::Reply(issued("d1"))]).await;
    let url = url::Url::parse(&server).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::write(paths.pending_claim(), b"{not json").unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(!crate::claim_pending(&paths, &url));
    assert_eq!(std::fs::read(paths.pending_claim()).unwrap(), b"{not json");

    let (account, _) = driver
        .claim(&server, "claim-code", "Ada", "laptop")
        .await
        .unwrap();

    assert_eq!(account.device_id, "d1");
    let ids = request_ids(&served.await.unwrap(), |r: ClaimRequest| r.request_id);
    assert_eq!(ids.len(), 1);
    assert!(!ids[0].is_empty());
    assert!(!paths.pending_claim().exists());
}

#[tokio::test]
async fn a_claim_whose_resends_end_without_an_answer_is_sent_again_with_its_first_request_id() {
    use crate::pending_create::fake::{Answer, long_wait, serve};

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let (server, served) =
        serve(vec![Answer::Lose, long_wait(), Answer::Reply(issued("d1"))]).await;

    let failed = driver.claim(&server, "claim-code", "Ada", "laptop").await;
    assert!(matches!(failed, Err(DriverError::Api(_))), "{failed:?}");
    assert!(crate::claim_pending(
        &paths,
        &url::Url::parse(&server).unwrap()
    ));
    let (account, _) = driver
        .claim(&server, "claim-code", "Ada", "laptop")
        .await
        .unwrap();

    assert_eq!(account.device_id, "d1");
    let ids = request_ids(&served.await.unwrap(), |r: ClaimRequest| r.request_id);
    assert_eq!(ids.len(), 3);
    assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
    assert_eq!(crate::pending_create::saved(&paths), 0);
}

#[tokio::test]
async fn a_pairing_after_a_refused_code_reports_what_the_folders_of_the_lost_credential_had_set_aside()
 {
    use crate::pending_create::fake::{Answer, serve};
    use koidrive_proto::v1::{Error as ProtoError, error::Code};
    for reopened in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Koidrive");
        linked(&paths, &root, "d1");
        parked_report(&paths, &root);
        fs::remove_file(paths.credential()).unwrap();
        let date = koidrive_fs::utc_date(SystemTime::now());
        let mut driver = Driver::open(paths.clone()).unwrap();
        let wrong = ProtoError {
            code: Code::Forbidden as i32,
            message: "invalid or expired pairing code".into(),
            ..ProtoError::default()
        };
        let (server, _served) = serve(vec![
            Answer::Refuse(403, wrong),
            Answer::Reply(issued("d2")),
        ])
        .await;

        assert!(driver.pair(&server, "ABCD-EFGH", "laptop").await.is_err());
        if reopened {
            drop(driver);
            driver = Driver::open(paths.clone()).unwrap();
        }
        let (account, set_aside) = driver.pair(&server, "WXYZ-WXYZ", "laptop").await.unwrap();
        if koidrive_fs::utc_date(SystemTime::now()) != date {
            eprintln!("SKIPPED: the UTC date changed during the test");
            return;
        }

        assert_eq!(account.device_id, "d2", "reopened: {reopened}");
        assert_eq!(
            set_aside.brought_back,
            vec![root.join(format!("report (recovered {date}).docx"))],
            "reopened: {reopened}"
        );
    }
}

#[tokio::test]
async fn a_pairing_whose_resends_end_without_an_answer_is_sent_again_with_its_first_request_id() {
    use crate::pending_create::fake::{Answer, long_wait, serve};
    use koidrive_proto::v1::{Error as ProtoError, error::Code};

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let wrong = ProtoError {
        code: Code::Forbidden as i32,
        message: "invalid or expired pairing code".into(),
        ..ProtoError::default()
    };
    let (server, served) = serve(vec![
        Answer::Refuse(403, wrong),
        Answer::Lose,
        long_wait(),
        Answer::Reply(issued("d1")),
    ])
    .await;

    let refused = driver.pair(&server, "ABCD-EFGH", "laptop").await;
    assert!(matches!(refused, Err(DriverError::Api(_))), "{refused:?}");
    assert_eq!(crate::pending_create::saved(&paths), 0);
    let failed = driver.pair(&server, "ABCD-EFGH", "laptop").await;
    assert!(matches!(failed, Err(DriverError::Api(_))), "{failed:?}");
    assert_eq!(crate::pending_create::saved(&paths), 1);
    let (account, _) = driver.pair(&server, "abcd efgh", "laptop").await.unwrap();

    assert_eq!(account.device_id, "d1");
    let ids = request_ids(&served.await.unwrap(), |r: PairCompleteRequest| {
        r.request_id
    });
    assert_eq!(ids.len(), 4);
    assert_ne!(ids[0], ids[1]);
    assert!(ids[1..].iter().all(|id| *id == ids[1]), "{ids:?}");
    assert_eq!(crate::pending_create::saved(&paths), 0);
}

#[tokio::test]
async fn a_mistyped_code_keeps_the_claim_that_did_not_finish() {
    use tokio::io::AsyncWriteExt;

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let url = url::Url::parse(&server).unwrap();
    let (arrived, first) = tokio::sync::oneshot::channel();
    let served = tokio::spawn(async move {
        let (mut held, _) = listener.accept().await.unwrap();
        let lost = read_request(&mut held).await.1;
        arrived.send(()).unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let refusal = prost::Message::encode_to_vec(&koidrive_proto::v1::Error {
            code: koidrive_proto::v1::error::Code::Forbidden as i32,
            message: "this deployment already has an owner".into(),
            ..Default::default()
        });
        let head = format!(
            "HTTP/1.1 403 Forbidden\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            refusal.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(&refusal).await.unwrap();
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        let resent = read_request(&mut socket).await.1;
        write_reply(&mut socket, &issued("d1")).await;
        drop(held);
        (lost, resent)
    });

    tokio::select! {
        outcome = driver.claim(&server, "claim-code", "Ada", "laptop") => panic!("the claim ended: {outcome:?}"),
        _ = first => {}
    }
    let mistyped = driver.claim(&server, "claim-cod", "Ada", "laptop").await;
    assert!(matches!(mistyped, Err(DriverError::Api(_))), "{mistyped:?}");
    assert!(crate::claim_pending(&paths, &url));
    driver
        .claim(&server, "claim-code", "Ada", "laptop")
        .await
        .unwrap();

    let (lost, resent) = served.await.unwrap();
    let lost: ClaimRequest = prost::Message::decode(lost.as_slice()).unwrap();
    let resent: ClaimRequest = prost::Message::decode(resent.as_slice()).unwrap();
    assert_eq!(resent.request_id, lost.request_id);
    assert_eq!(crate::pending_create::saved(&paths), 0);
}

#[tokio::test]
async fn a_refused_claim_is_not_resumed() {
    use tokio::io::AsyncWriteExt;

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for reply in [None, Some(issued("d1"))] {
            let (mut socket, _) = listener.accept().await.unwrap();
            bodies.push(read_request(&mut socket).await.1);
            match reply {
                Some(reply) => write_reply(&mut socket, &reply).await,
                None => {
                    let refusal = prost::Message::encode_to_vec(&koidrive_proto::v1::Error {
                        code: koidrive_proto::v1::error::Code::Forbidden as i32,
                        message: "wrong claim code".into(),
                        ..Default::default()
                    });
                    let head = format!(
                        "HTTP/1.1 403 Forbidden\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        refusal.len()
                    );
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(&refusal).await.unwrap();
                }
            }
        }
        bodies
    });

    let refused = driver.claim(&server, "claim-code", "Ada", "laptop").await;
    assert!(matches!(refused, Err(DriverError::Api(_))), "{refused:?}");
    assert_eq!(crate::pending_create::saved(&paths), 0);
    driver
        .claim(&server, "claim-code", "Ada", "laptop")
        .await
        .unwrap();

    let ids: Vec<String> = (served.await.unwrap().iter())
        .map(|body| {
            prost::Message::decode(body.as_slice())
                .map(|request: ClaimRequest| request.request_id)
                .unwrap()
        })
        .collect();
    assert_ne!(ids[0], ids[1]);
}

async fn request_ids_around_a_refusal(
    status: u16,
    refusal: Option<(koidrive_proto::v1::error::Code, i32)>,
) -> (bool, Vec<String>) {
    use tokio::io::AsyncWriteExt;

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let (content_type, refusal) = match refusal {
        Some((code, cause)) => (
            "application/x-protobuf",
            prost::Message::encode_to_vec(&koidrive_proto::v1::Error {
                code: code as i32,
                message: "refused".into(),
                cause,
                ..Default::default()
            }),
        ),
        None => ("text/plain", b"error code: 1020".to_vec()),
    };
    let refused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let first_claim_ended = refused.clone();
    let served = tokio::spawn(async move {
        let mut bodies = Vec::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            bodies.push(read_request(&mut socket).await.1);
            if refused.load(std::sync::atomic::Ordering::SeqCst) {
                write_reply(&mut socket, &issued("d1")).await;
                return bodies;
            }
            let head = format!(
                "HTTP/1.1 {status} Refused\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                refusal.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&refusal).await.unwrap();
        }
    });

    let failed = driver.claim(&server, "claim-code", "Ada", "laptop").await;
    assert!(matches!(failed, Err(DriverError::Api(_))), "{failed:?}");
    first_claim_ended.store(true, std::sync::atomic::Ordering::SeqCst);
    let pending = crate::claim_pending(&paths, &url::Url::parse(&server).unwrap());
    driver
        .claim(&server, "claim-code", "Ada", "laptop")
        .await
        .unwrap();

    let ids = (served.await.unwrap().iter())
        .map(|body| {
            prost::Message::decode(body.as_slice())
                .map(|request: ClaimRequest| request.request_id)
                .unwrap()
        })
        .collect();
    (pending, ids)
}

#[tokio::test]
async fn a_claim_keeps_its_request_id_through_a_429_an_unreadable_answer_or_any_5xx() {
    use koidrive_proto::v1::error::{Cause, Code};

    let answers = tokio::join!(
        request_ids_around_a_refusal(500, Some((Code::Internal, Cause::DoUnavailable as i32))),
        request_ids_around_a_refusal(500, Some((Code::Internal, Cause::Exception as i32))),
        request_ids_around_a_refusal(500, Some((Code::Internal, Cause::Unspecified as i32))),
        request_ids_around_a_refusal(429, Some((Code::QuotaExceeded, Cause::DailyLimit as i32))),
        request_ids_around_a_refusal(429, Some((Code::RetryLater, Cause::Unspecified as i32))),
        request_ids_around_a_refusal(403, None),
    );

    for (pending, ids) in [
        answers.0, answers.1, answers.2, answers.3, answers.4, answers.5,
    ] {
        assert!(
            pending,
            "an answer that allows a resend forgot the pending claim"
        );
        assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
    }
}

#[tokio::test]
async fn a_paired_device_does_not_know_the_user_name() {
    let dir = tempfile::tempdir().unwrap();
    let driver = Driver::open(Paths::under(dir.path())).unwrap();
    let (server, served) = serve_once(issued("d2")).await;
    let (account, _) = driver.pair(&server, "pair-code", "desktop").await.unwrap();
    assert_eq!(served.await.unwrap().0, "/v1/pair/complete");
    assert_eq!(account.user_name, None);
    assert_eq!(account.device_name, "desktop");
}

#[tokio::test]
async fn a_pause_stops_every_folder_outlives_the_driver_and_only_a_resume_ends_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let state = |driver: &Driver| driver.status().folders[0].state;
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    assert_ne!(state(&driver), FolderState::Stopped);
    let mut watch = driver.watch();
    watch.borrow_and_update();

    assert!(driver.pause().await.unwrap());
    assert!(watch.has_changed().unwrap());
    assert!(!driver.pause().await.unwrap());
    let status = driver.status();
    assert!(status.running && status.paused, "{status:?}");
    assert_eq!(status.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(state(&driver), FolderState::Stopped);
    driver.restart(&root).await.unwrap();
    assert_eq!(state(&driver), FolderState::Stopped);
    drop(driver);

    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    assert!(driver.status().paused);
    assert_eq!(state(&driver), FolderState::Stopped);
    assert!(driver.resume().await.unwrap());
    assert!(!driver.resume().await.unwrap());
    assert!(!paths.paused().exists());
    assert!(!driver.status().paused);
    assert_ne!(state(&driver), FolderState::Stopped);
    driver.stop().await;
    drop(driver);
    assert!(!Driver::open(paths).unwrap().status().paused);
}
