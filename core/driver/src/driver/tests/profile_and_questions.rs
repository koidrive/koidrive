// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_profile_opens_again_while_a_child_process_holds_a_copy_of_its_locks_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let _child = claim::tests::Holder::of(&driver.inner._lock);
    drop(driver);
    Driver::open(paths).unwrap();
}

#[test]
fn linked_folders_are_found_in_their_stores() {
    let (_dir, paths, root) = profile(&[mass_delete("q1")]);
    let status = Driver::open(paths).unwrap().status();
    assert_eq!(status.account, None);
    assert!(!status.running);
    assert_eq!(status.folders.len(), 1);
    let folder = &status.folders[0];
    assert_eq!(folder.space_id, SPACE);
    assert_eq!(folder.path, Some(root.display().to_string()));
    assert_eq!(folder.state, FolderState::Stopped);
    assert_eq!(folder.questions, 1);
}

#[test]
fn questions_take_only_the_answers_they_offer() {
    let (_dir, paths, _root) = profile(&[mass_delete("q1"), mass_delete("q2")]);
    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(
        driver.questions().unwrap(),
        [mass_delete("q1"), mass_delete("q2")]
    );

    assert!(matches!(
        driver.answer("q1", Answer::Publish),
        Err(DriverError::AnswerNotAllowed {
            allowed: [Answer::Delete, Answer::Restore]
        })
    ));
    assert_eq!(driver.answer("q1", Answer::Restore).unwrap(), Answered::Now);
    assert!(matches!(
        driver.answer("q1", Answer::Delete),
        Err(DriverError::AnsweredOtherwise {
            given: Answer::Restore
        })
    ));
    assert_eq!(driver.questions().unwrap(), [mass_delete("q2")]);
    assert_eq!(driver.status().folders[0].questions, 1);

    let stored = driver.stored_questions(SPACE).unwrap().unwrap();
    assert_eq!(stored[0].answer.as_ref().unwrap().answer, "restore");
}

#[test]
fn the_same_answer_again_changes_nothing_until_the_engine_drops_the_question_without_it() {
    let (_dir, paths, _root) = profile(&[mass_delete("q1"), mass_delete("q2")]);
    let driver = Driver::open(paths.clone()).unwrap();
    let store = || Store::open(paths.store(SPACE).unwrap()).unwrap();
    assert_eq!(driver.answer("q1", Answer::Delete).unwrap(), Answered::Now);
    assert_eq!(
        driver.answer("q1", Answer::Delete).unwrap(),
        Answered::Already
    );

    assert!(store().transaction(|tx| tx.take_answer("q1")).unwrap());
    assert_eq!(
        driver.answer("q1", Answer::Delete).unwrap(),
        Answered::Already
    );
    assert!(matches!(
        driver.answer("q1", Answer::Restore),
        Err(DriverError::AnsweredOtherwise {
            given: Answer::Delete
        })
    ));

    assert_eq!(driver.answer("q2", Answer::Delete).unwrap(), Answered::Now);
    store()
        .transaction(|tx| {
            tx.remove_question("q2")?;
            tx.put_question(&StoredQuestion {
                question_id: "q3".into(),
                asked_at_ms: 20,
                body: serde_json::to_vec(&mass_delete("q3")).unwrap(),
                answer: None,
            })
        })
        .unwrap();
    assert!(matches!(
        driver.answer("q2", Answer::Delete),
        Err(DriverError::QuestionNotFound(_))
    ));
    assert_eq!(driver.questions().unwrap(), [mass_delete("q3")]);
}

#[test]
fn a_question_asked_again_after_the_engine_took_its_answer_reads_as_unanswered() {
    let (_dir, paths, _root) = profile(&[mass_delete("q1")]);
    let driver = Driver::open(paths.clone()).unwrap();
    let store = || Store::open(paths.store(SPACE).unwrap()).unwrap();
    assert_eq!(driver.answer("q1", Answer::Delete).unwrap(), Answered::Now);
    assert!(store().transaction(|tx| tx.take_answer("q1")).unwrap());
    assert_eq!(
        driver.answered_question("q1").unwrap(),
        Some(mass_delete("q1"))
    );

    store()
        .transaction(|tx| {
            tx.put_question(&StoredQuestion {
                question_id: "q1".into(),
                asked_at_ms: 30,
                body: serde_json::to_vec(&mass_delete("q1")).unwrap(),
                answer: None,
            })
        })
        .unwrap();
    assert_eq!(driver.answered_question("q1").unwrap(), None);
    assert_eq!(driver.questions().unwrap(), [mass_delete("q1")]);
    assert_eq!(driver.answer("q1", Answer::Restore).unwrap(), Answered::Now);
}

#[tokio::test]
async fn the_same_answer_again_changes_nothing_after_the_folder_took_it() {
    use koidrive_engine::question::{ConflictFile, Revisions};

    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let file = |node_id: &str| ConflictFile {
        node_id: node_id.into(),
        revisions: Revisions {
            content_rev: 2,
            meta_rev: 1,
        },
    };
    let kind = QuestionKind::Conflict {
        original: file("n-original"),
        copy: file("n-copy"),
        sent_in: None,
    };
    let shown = Question {
        recovery_cause: None,
        id: "c1".into(),
        lineage: "c1".into(),
        space_id: SPACE.into(),
        asked_at_ms: 10,
        kind: kind.clone(),
        items: Vec::new(),
    };
    Store::open(paths.store(SPACE).unwrap())
        .unwrap()
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            state.commits.questions = vec![koidrive_engine::question::Question {
                id: "c1".into(),
                asked_at_ms: 10,
                kind,
                held: BTreeMap::new(),
                answer: None,
                shown: BTreeMap::new(),
            }];
            tx.set_engine_state(&state)?;
            tx.put_question(&StoredQuestion {
                question_id: "c1".into(),
                asked_at_ms: 10,
                body: serde_json::to_vec(&shown).unwrap(),
                answer: None,
            })
        })
        .unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(
        driver.answer("c1", Answer::KeepBoth).unwrap(),
        Answered::Now
    );

    driver.start();
    eventually("the folder takes the answer", || {
        driver.stored_questions(SPACE).unwrap().unwrap().is_empty()
    })
    .await;
    assert_eq!(
        driver.answer("c1", Answer::KeepBoth).unwrap(),
        Answered::Already
    );
    assert!(matches!(
        driver.answer("c1", Answer::KeepCopy),
        Err(DriverError::AnsweredOtherwise {
            given: Answer::KeepBoth
        })
    ));
    driver.stop().await;
}

#[test]
fn a_conflict_stored_without_its_versions_reads_them_from_the_remote_tree() {
    use koidrive_engine::question::{ConflictFile, Revisions};
    use koidrive_proto::v1::{Content, NodeState, VersionInfo};

    let file = |node_id: &str| ConflictFile {
        node_id: node_id.into(),
        revisions: Revisions {
            content_rev: 1,
            meta_rev: 1,
        },
    };
    let item = |node_id: &str, path: &str, device_id: &str| HeldItem {
        node_id: node_id.into(),
        path: path.into(),
        kind: None,
        files: 1,
        device_id: Some(device_id.into()),
        device_name: None,
        size: None,
        modified_at_ms: None,
        restore: None,
    };
    let stored = Question {
        recovery_cause: None,
        id: "c1".into(),
        lineage: "c1".into(),
        space_id: SPACE.into(),
        asked_at_ms: 10,
        kind: QuestionKind::Conflict {
            original: file("n-original"),
            copy: file("n-copy"),
            sent_in: None,
        },
        items: vec![
            item("n-copy", "notes (conflict).txt", "d1"),
            item("n-original", "notes.txt", "d1"),
        ],
    };
    let (_dir, paths, _root) = profile(&[stored.clone(), mass_delete("q1")]);
    Store::open(paths.store(SPACE).unwrap())
        .unwrap()
        .transaction(|tx| {
            tx.put_remote_node(&NodeState {
                node_id: "n-original".into(),
                name: "notes.txt".into(),
                current: Some(VersionInfo {
                    device_id: "d2".into(),
                    content: Some(Content {
                        size: 1_024,
                        mtime_ns: 1_794_442_400_123_456_789,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            })
        })
        .unwrap();

    let driver = Driver::open(paths).unwrap();
    let mut expected = stored;
    expected.items[0].device_id = None;
    expected.items[1] = HeldItem {
        device_id: Some("d2".into()),
        size: Some(1_024),
        modified_at_ms: Some(1_794_442_400_123),
        restore: None,
        ..item("n-original", "notes.txt", "d2")
    };
    assert_eq!(driver.questions().unwrap(), [expected, mass_delete("q1")]);
}

fn conflict_written_by_d1_and_d2(paths: &Paths) {
    use koidrive_engine::question::{ConflictFile, Revisions};
    use koidrive_proto::v1::{NodeState, VersionInfo};

    let file = |node_id: &str| ConflictFile {
        node_id: node_id.into(),
        revisions: Revisions {
            content_rev: 1,
            meta_rev: 1,
        },
    };
    let item = |node_id: &str, path: &str| HeldItem {
        node_id: node_id.into(),
        path: path.into(),
        kind: None,
        files: 1,
        device_id: None,
        device_name: None,
        size: None,
        modified_at_ms: None,
        restore: None,
    };
    let conflict = Question {
        recovery_cause: None,
        id: "c1".into(),
        lineage: "c1".into(),
        space_id: SPACE.into(),
        asked_at_ms: 10,
        kind: QuestionKind::Conflict {
            original: file("n-original"),
            copy: file("n-copy"),
            sent_in: None,
        },
        items: vec![
            item("n-copy", "notes (conflict).txt"),
            item("n-original", "notes.txt"),
        ],
    };
    Store::open(paths.store(SPACE).unwrap())
        .unwrap()
        .transaction(|tx| {
            for (node_id, device_id) in [("n-copy", "d1"), ("n-original", "d2")] {
                tx.put_remote_node(&NodeState {
                    node_id: node_id.into(),
                    current: Some(VersionInfo {
                        device_id: device_id.into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })?;
            }
            tx.put_question(&StoredQuestion {
                question_id: "c1".into(),
                asked_at_ms: 10,
                body: serde_json::to_vec(&conflict).unwrap(),
                answer: None,
            })
        })
        .unwrap();
}

fn device_list() -> (u16, Vec<u8>) {
    use koidrive_proto::v1::{DeviceInfo, DeviceList};

    let devices = DeviceList {
        devices: vec![
            DeviceInfo {
                device_id: "d1".into(),
                name: "Ada's MacBook Air".into(),
                ..Default::default()
            },
            DeviceInfo {
                device_id: "d2".into(),
                name: "agent-box".into(),
                revoked_at_ms: 5,
                ..Default::default()
            },
        ],
        next_cursor: String::new(),
    };
    (200, prost::Message::encode_to_vec(&devices))
}

fn writers(driver: &Driver) -> Vec<(String, Option<String>)> {
    (driver.questions().unwrap().into_iter())
        .flat_map(|question| question.items)
        .map(|item| (item.device_id.unwrap(), item.device_name))
        .collect()
}

fn named() -> [(String, Option<String>); 2] {
    [
        ("d1".to_owned(), Some("Ada's MacBook Air".to_owned())),
        ("d2".to_owned(), Some("agent-box".to_owned())),
    ]
}

#[tokio::test]
async fn a_conflict_names_the_devices_that_wrote_its_files_once_the_account_device_list_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let (server, served) = serve_each(
        vec![access_token(), device_list()],
        Paths::under(&base.join("profile")).credential(),
    )
    .await;
    let paths = linked_with_server(&base, server);
    conflict_written_by_d1_and_d2(&paths);

    let driver = Driver::open(paths).unwrap();
    let mut watch = driver.watch();
    watch.borrow_and_update();
    assert_eq!(
        writers(&driver),
        [("d1".to_owned(), None), ("d2".to_owned(), None)]
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while writers(&driver) != named() {
            watch.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(writers(&driver), named());
    let requested: Vec<String> = (served.await.unwrap().into_iter())
        .map(|(path, _)| path)
        .collect();
    assert_eq!(requested, ["/v1/token", "/v1/devices"]);
}

#[tokio::test]
async fn listing_the_writers_names_them_on_the_first_read() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let (server, served) = serve_each(
        vec![access_token(), device_list()],
        Paths::under(&base.join("profile")).credential(),
    )
    .await;
    let paths = linked_with_server(&base, server);
    conflict_written_by_d1_and_d2(&paths);

    let questions = Driver::open(paths).unwrap().into_questions().await.unwrap();
    let writers: Vec<_> = (questions.into_iter())
        .flat_map(|question| question.items)
        .map(|item| (item.device_id.unwrap(), item.device_name))
        .collect();
    assert_eq!(writers, named());
    assert_eq!(served.await.unwrap().len(), 2);
}

async fn hanging_device_list() -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<()>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let (listed, listing) = tokio::sync::mpsc::unbounded_channel();
    let served = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (path, _) = read_request(&mut socket).await;
            if path == "/v1/devices" {
                held.push(socket);
                listed.send(()).unwrap();
                continue;
            }
            let (status, body) = access_token();
            let head = format!(
                "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        }
    });
    (server, listing, served)
}

#[tokio::test]
async fn a_device_listing_that_hangs_leaves_the_profile_free_once_the_driver_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let (server, mut listing, served) = hanging_device_list().await;
    let paths = linked_with_server(&base, server);
    conflict_written_by_d1_and_d2(&paths);

    let driver = Driver::open(paths.clone()).unwrap();
    assert_eq!(
        writers(&driver),
        [("d1".to_owned(), None), ("d2".to_owned(), None)]
    );
    listing.recv().await.unwrap();
    drop(driver);
    let reopened = Driver::open(paths).unwrap();
    assert_eq!(writers(&reopened).len(), 2);
    served.abort();
}

#[tokio::test]
async fn questions_read_once_free_the_profile_while_the_device_list_hangs() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let (server, mut listing, served) = hanging_device_list().await;
    let paths = linked_with_server(&base, server);
    conflict_written_by_d1_and_d2(&paths);

    let reading = tokio::spawn(Driver::open(paths.clone()).unwrap().into_questions());
    listing.recv().await.unwrap();
    drop(Driver::open(paths).unwrap());
    let writers: Vec<_> = (reading.await.unwrap().unwrap().into_iter())
        .flat_map(|question| question.items)
        .map(|item| (item.device_id.unwrap(), item.device_name))
        .collect();
    assert_eq!(writers, [("d1".to_owned(), None), ("d2".to_owned(), None)]);
    served.abort();
}

#[tokio::test]
async fn questions_read_once_never_rotate_the_credential_of_the_process_that_takes_the_profile() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let (asked, mut asking) = tokio::sync::mpsc::unbounded_channel::<()>();
    let gate = Arc::new(tokio::sync::Notify::new());
    let released = gate.clone();
    let served = tokio::spawn(async move {
        let mut current = "secret".to_owned();
        let mut first = true;
        let mut held = Vec::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (path, body) = read_request(&mut socket).await;
            if path.contains("/v1/devices") {
                held.push(socket);
                continue;
            }
            let request = token_request(&body);
            if first {
                first = false;
                asked.send(()).unwrap();
                released.notified().await;
            }
            let (status, reply) = if request.refresh_credential != current {
                let refusal = koidrive_proto::v1::Error {
                    code: koidrive_proto::v1::error::Code::Unauthenticated.into(),
                    message: "unknown credential".into(),
                    ..Default::default()
                };
                (401, prost::Message::encode_to_vec(&refusal))
            } else {
                let rotated = !request.next_refresh_credential.is_empty();
                if rotated {
                    current = request.next_refresh_credential.clone();
                }
                let token = koidrive_proto::v1::AccessToken {
                    token: "token".into(),
                    expires_at_ms: i64::MAX,
                    server_time_ms: unix_ms_now(),
                    rotated,
                };
                (200, prost::Message::encode_to_vec(&token))
            };
            let head = format!(
                "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            let response = [head.as_bytes(), &reply].concat();
            tokio::io::AsyncWriteExt::write_all(&mut socket, &response)
                .await
                .unwrap();
        }
    });
    let paths = linked_with_server(&base, server);
    conflict_written_by_d1_and_d2(&paths);
    fs::File::options()
        .write(true)
        .open(paths.credential())
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(2 * 86_400))
        .unwrap();

    let saved = fs::read(paths.credential()).unwrap();
    let reading = tokio::spawn(Driver::open(paths.clone()).unwrap().into_questions());
    asking.recv().await.unwrap();
    let daemon =
        Driver::open(paths.clone()).expect("the profile is free while the one-shot read refreshes");
    let auth = daemon.signed_in().unwrap();
    gate.notify_one();
    reading.await.unwrap().unwrap();
    assert_eq!(fs::read(paths.credential()).unwrap(), saved);
    let answer = auth.access_token().await;
    served.abort();
    assert!(
        answer.is_ok(),
        "the process that holds the profile was refused: {answer:?}"
    );
}

#[tokio::test]
async fn a_folder_linked_by_another_credential_does_not_sync() {
    let (_dir, paths, _root) = profile(&[]);
    credential("d2").save_new(&paths.credential()).unwrap();
    let driver = Driver::open(paths).unwrap();
    driver.start();
    let status = driver.status();
    assert!(status.running);
    assert_eq!(status.account.unwrap().device_id, "d2");
    let folder = &status.folders[0];
    assert_eq!(folder.state, FolderState::Halted);
    assert!(
        folder.problem.as_ref().unwrap().contains("device d1"),
        "{folder:?}"
    );
    driver.stop().await;
    assert_eq!(driver.status().folders[0].state, FolderState::Halted);
}
