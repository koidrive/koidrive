// SPDX-License-Identifier: Apache-2.0

use crate::failures::{self, Failure};
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use crate::leave::Intent;
use crate::question::{HeldItem, QuestionKind};
use crate::restore::{RestoreRejection, RestoreState, RestoreWait};

mod root_token;

use super::link::{
    UNLINK_BROUGHT_BACK, held_aside, record_unreported_set_aside, recorded_brought_back,
    resolve_space,
};
use super::questions::decode_question;
use super::*;

const SPACE: &str = "0190f0a0-0000-7000-8000-000000000001";

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

fn credential(device_id: &str) -> Credential {
    Credential {
        server: "https://files.example.com/".into(),
        user_id: "u1".into(),
        user_name: None,
        device_id: device_id.into(),
        device_name: "vm".into(),
        refresh_credential: "secret".into(),
    }
}

fn release_driver(paths: Paths) -> Driver {
    let mut driver = Driver::open(paths).unwrap();
    let inner = Arc::get_mut(&mut driver.inner).unwrap();
    let config = Config::load(&inner.paths.config_file).unwrap_or_default();
    let variable = std::env::var(update::VARIABLE).ok();
    inner.updates = Arc::new(Updates::for_version(
        update::switched_on(&config.updates, variable.as_deref()),
        inner.paths.update_check(),
        semver::Version::parse("0.1.0").unwrap(),
    ));
    driver
}

fn profile(questions: &[Question]) -> (tempfile::TempDir, Paths, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    fs::create_dir_all(&root).unwrap();
    let store_path = paths.store(SPACE).unwrap();
    fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    let mut store = Store::open(&store_path).unwrap();
    store
        .transaction(|tx| {
            tx.set_link(&Link {
                space_id: SPACE.into(),
                space_name: "Projects".into(),
                root: root.clone(),
                root_dev: 1,
                root_volume: None,
                server_url: "https://files.example.com/".into(),
                device_id: "d1".into(),
                linked_at_ms: 1,
                found_marker: false,
            })?;
            for question in questions {
                tx.put_question(&StoredQuestion {
                    question_id: question.id.clone(),
                    asked_at_ms: question.asked_at_ms,
                    body: serde_json::to_vec(question).unwrap(),
                    answer: None,
                })?;
            }
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
    (dir, paths, root)
}

fn mass_delete(id: &str) -> Question {
    Question {
        recovery_cause: None,
        id: id.into(),
        lineage: id.into(),
        space_id: SPACE.into(),
        asked_at_ms: 10,
        kind: QuestionKind::MassDelete {
            deletions: 1_001,
            affected_files: 1_001,
            space_files: 5_000,
            exceeded: vec![crate::question::Limit::Deletions],
        },
        items: vec![HeldItem {
            node_id: "n1".into(),
            path: "old".into(),
            kind: None,
            files: 1_001,
            device_id: None,
            device_name: None,
            size: None,
            modified_at_ms: None,
            restore: None,
        }],
    }
}

#[test]
fn one_process_at_a_time_opens_a_profile() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(matches!(
        Driver::open(paths.clone()),
        Err(DriverError::Busy(_))
    ));
    drop(driver);
    Driver::open(paths).unwrap();
}

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

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
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
        assert_eq!(broken.path, Some(root.display().to_string()));
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

fn nowhere() -> &'static str {
    static URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    URL.get_or_init(|| format!("http://{}/", koidrive_test_support::reset_tcp_address()))
}

fn paired(paths: &Paths, device_id: &str) {
    Credential {
        server: nowhere().into(),
        ..credential(device_id)
    }
    .save_new(&paths.credential())
    .unwrap();
}

fn linked(paths: &Paths, root: &Path, device_id: &str) {
    paired(paths, device_id);
    linked_to(paths, root, device_id, SPACE);
}

fn linked_to(paths: &Paths, root: &Path, device_id: &str, space_id: &str) {
    fs::create_dir_all(root).unwrap();
    write_marker(
        root,
        &Marker {
            space_id: space_id.into(),
            effects: 0,
            token: None,
        },
    )
    .unwrap();
    let store_path = paths.store(space_id).unwrap();
    fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    let link = Link {
        space_id: space_id.into(),
        space_name: "Projects".into(),
        root: root.to_path_buf(),
        root_dev: fs::metadata(root).unwrap().dev(),
        root_volume: None,
        server_url: nowhere().into(),
        device_id: device_id.into(),
        linked_at_ms: 1,
        found_marker: false,
    };
    Store::open(&store_path)
        .unwrap()
        .transaction(|tx| tx.set_link(&link))
        .unwrap();
}

/// Waits until `done`. No deadline fails it, since a loaded machine can take any time: a wait that never ends
/// names `what` each minute, and the CI job's timeout stops it.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let mut waited = Instant::now();
    while !done() {
        if waited.elapsed() >= Duration::from_secs(60) {
            eprintln!("still waiting: {what}");
            waited = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn halted(driver: &Driver) -> String {
    eventually("the folder halts", || {
        driver.status().folders[0].state == FolderState::Halted
    })
    .await;
    driver.status().folders[0].problem.clone().unwrap()
}

#[tokio::test]
async fn the_folders_of_a_profile_share_one_limit_on_uploads_in_flight() {
    use koidrive_proto::v1::ChunkRef;

    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    paired(&paths, "d1");
    linked_to(&paths, &base.join("A"), "d1", SPACE);
    linked_to(
        &paths,
        &base.join("B"),
        "d1",
        "0190f0a0-0000-7000-8000-000000000002",
    );
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    fs::write(&paths.config_file, "[upload]\nconcurrency = 3\n").unwrap();
    let driver = Driver::open(paths).unwrap();
    let credential = driver.credential().unwrap();
    let tasks: Vec<_> = (driver.inner.folders.lock().unwrap().values())
        .map(|folder| driver.task(folder, &credential, Vec::new()).unwrap())
        .collect();
    assert_eq!(tasks.len(), 2);

    let (relay, seen) = crate::scripted::relay(3).await;
    let uploads = tasks.iter().enumerate().map(|(folder, task)| async move {
        let (session, _events, _) = crate::scripted::session(relay).await;
        let chunks: Vec<_> = (0..6)
            .map(|i| {
                let mut bytes = format!("chunk {i} of folder {folder}").into_bytes();
                bytes.resize(koidrive_net::FULL_CHUNK_BYTES as usize - 64, 0);
                let chunk = ChunkRef {
                    hash: koidrive_chunk::chunk_hash(&bytes).to_vec(),
                    size: bytes.len() as u32,
                    ..Default::default()
                };
                (chunk, bytes::Bytes::from(bytes))
            })
            .collect();
        let refs: Vec<_> = chunks.iter().map(|(chunk, _)| chunk.clone()).collect();
        let read = |wanted: &ChunkRef| {
            let found = chunks.iter().find(|(chunk, _)| chunk == wanted).unwrap();
            std::future::ready(Ok(found.1.clone()))
        };
        task.transfer().upload(&session, &refs, read).await.unwrap()
    });
    let receipts = futures_util::future::join_all(uploads).await;

    assert_eq!(receipts.iter().map(Vec::len).sum::<usize>(), 12);
    assert_eq!(seen.lock().unwrap().peak, 3);
}

#[tokio::test]
async fn a_folder_that_holds_a_reopened_profile_halts_before_it_is_observed() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap().join("Koidrive");
    let paths = Paths::under(&root.join("moved-profile"));
    linked(&paths, &root, "d1");
    fs::write(root.join("file"), b"synced").unwrap();

    let driver = Driver::open(paths).unwrap();
    driver.start();
    let problem = halted(&driver).await;
    assert!(
        problem.contains("overlaps Koidrive's own files"),
        "{problem}"
    );
    assert_eq!(driver.status().folders[0].local_entries, None);
    assert_eq!(claim::owner(&root).unwrap(), None);
    driver.stop().await;
}

#[test]
fn an_overlap_is_found_through_a_symbolic_link() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let state = root.join("profile/state");
    fs::create_dir_all(&state).unwrap();
    let alias = base.join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    assert!(matches!(
        check_placement(&alias, &[state], &[]),
        Err(DriverError::FolderHoldsProfile(_))
    ));
    assert!(matches!(
        check_placement(&root.join("profile"), &[], &[alias]),
        Err(DriverError::FolderOverlaps(_))
    ));
}

#[tokio::test]
async fn two_profiles_never_sync_one_folder_at_once_or_in_turn() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let (a, b) = (Paths::under(&base.join("a")), Paths::under(&base.join("b")));
    linked(&a, &root, "d1");
    linked(&b, &root, "d2");
    fs::write(root.join("one"), b"1").unwrap();
    let first = Driver::open(a).unwrap();
    let second = Driver::open(b).unwrap();
    let entries = |driver: &Driver| driver.status().folders[0].local_entries;

    first.start();
    eventually("the first profile observes the folder", || {
        entries(&first) == Some(1)
    })
    .await;
    second.start();
    let problem = halted(&second).await;
    assert!(problem.contains("another Koidrive process"), "{problem}");
    second.stop().await;

    first.stop().await;
    drop(
        claim::lock(&root)
            .unwrap()
            .expect("stopping releases the folder's lock"),
    );
    fs::write(root.join("two"), b"2").unwrap();
    first.start();
    eventually("the restarted profile observes the folder", || {
        entries(&first) == Some(2)
    })
    .await;
    assert_ne!(first.status().folders[0].state, FolderState::Halted);
    second.start();
    let problem = halted(&second).await;
    assert!(problem.contains("another Koidrive process"), "{problem}");
    second.stop().await;
    first.stop().await;

    second.start();
    assert!(halted(&second).await.contains("another Koidrive process"));
    second.restart(&root).await.unwrap();
    let problem = halted(&second).await;
    assert!(problem.contains("linked by device d1"), "{problem}");
    second.stop().await;
    assert!(claim::lock(&root).unwrap().is_some());
}

#[tokio::test]
async fn deleting_the_private_directory_does_not_let_another_profile_in() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let (a, b) = (Paths::under(&base.join("a")), Paths::under(&base.join("b")));
    linked(&a, &root, "d1");
    linked(&b, &root, "d2");
    fs::write(root.join("one"), b"1").unwrap();
    let first = Driver::open(a).unwrap();
    first.start();
    eventually("the first profile observes the folder", || {
        first.status().folders[0].local_entries == Some(1)
    })
    .await;

    fs::remove_dir_all(root.join(koidrive_fs::PRIVATE_DIR)).unwrap();
    let third = Paths::under(&base.join("c"));
    paired(&third, "d3");
    assert!(matches!(
        Driver::open(third).unwrap().link(&root, "Projects").await,
        Err(DriverError::FolderInUse(_))
    ));
    write_marker(
        &root,
        &Marker {
            space_id: SPACE.into(),
            effects: 0,
            token: None,
        },
    )
    .unwrap();
    let second = Driver::open(b).unwrap();
    second.start();
    let problem = halted(&second).await;
    assert!(problem.contains("another Koidrive process"), "{problem}");
    assert_ne!(first.status().folders[0].state, FolderState::Halted);
    second.stop().await;
    first.stop().await;
}

#[tokio::test]
async fn a_folder_replaced_under_its_task_halts_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let driver = Driver::open(paths).unwrap();
    driver.start();
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;

    fs::rename(&root, base.join("Moved")).unwrap();
    fs::create_dir(&root).unwrap();
    let problem = halted(&driver).await;
    assert!(problem.contains("no longer the folder"), "{problem}");
    driver.stop().await;
    assert!(claim::lock(&base.join("Moved")).unwrap().is_some());
}

#[tokio::test]
async fn stopping_does_not_wait_for_the_first_scan_to_read_every_file() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    for n in 0..16 {
        let file = fs::File::create(root.join(format!("large-{n}"))).unwrap();
        file.set_len(1 << 40).unwrap();
    }
    let mut driver = Driver::open(paths).unwrap();
    let (entered, scanning) = tokio::sync::oneshot::channel();
    Arc::get_mut(&mut driver.inner)
        .unwrap()
        .rules
        .get_mut()
        .unwrap()
        .matcher = Arc::new(Paused {
        entered: Mutex::new(Some(entered)),
        gate: Arc::new((Mutex::new(true), std::sync::Condvar::new())),
    });
    driver.start();
    scanning.await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    driver.stop().await;
}

#[tokio::test]
async fn stopping_does_not_wait_out_the_observers_pause_after_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let mut driver = Driver::open(paths).unwrap();
    let (paused, pausing) = tokio::sync::oneshot::channel();
    Arc::get_mut(&mut driver.inner).unwrap().observe_fault =
        Some(Arc::new(crate::folder::ObserveFault {
            paused: Mutex::new(Some(paused)),
        }));
    driver.start();
    pausing.await.unwrap();

    driver.stop().await;
    assert!(lockable(&root), "the observer's thread released the folder");
}

#[tokio::test]
async fn a_folder_moved_away_and_back_syncs_again() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let driver = Driver::open(paths).unwrap();
    driver.start();
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;

    fs::rename(&root, base.join("Moved")).unwrap();
    let problem = halted(&driver).await;
    assert!(problem.contains("no longer"), "{problem}");
    fs::rename(base.join("Moved"), &root).unwrap();
    eventually("the folder syncs again", || {
        driver.status().folders[0].state != FolderState::Halted
            && claim::lock(&root).unwrap().is_none()
    })
    .await;
    driver.stop().await;
    assert!(claim::lock(&root).unwrap().is_some());
}

#[tokio::test]
async fn a_halted_folder_shows_in_the_next_hourly_report_and_its_recovery_after() {
    use crate::health::tests::{named, posthog, with_prefix};

    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let recorder = koidrive_telemetry::Recorder::on();
    let driver = Driver::open_with_telemetry(paths, recorder.clone()).unwrap();
    driver.start();
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;
    driver.record_health();
    let healthy = posthog(recorder.take());
    assert_eq!(named(&healthy, "hourly_report")[0]["l.sync.halted"], 0);

    fs::rename(&root, base.join("Moved")).unwrap();
    halted(&driver).await;
    let now = koidrive_telemetry::now();
    driver.record_health_at(now);
    let events = posthog(recorder.take());
    let report = named(&events, "hourly_report")[0];
    assert_eq!(report["l.sync.halted"], 1);
    assert_eq!(report["l.sync.oldest_halt_minutes"], 0);
    let reasons = with_prefix(report, "l.halted.");
    let [(reason, 1)] = reasons[..] else {
        panic!("{reasons:?}")
    };
    let reason = reason.trim_start_matches("l.halted.").to_owned();
    assert!(named(&events, "problem_cleared").is_empty());

    fs::rename(base.join("Moved"), &root).unwrap();
    eventually("the folder syncs again", || {
        driver.status().folders[0].state != FolderState::Halted
    })
    .await;
    driver.record_health_at(now + 3_600);
    let events = posthog(recorder.take());
    let report = named(&events, "hourly_report")[0];
    assert_eq!(report["l.sync.halted"], 0);
    assert!(with_prefix(report, "l.halted.").is_empty(), "{report}");
    let cleared = named(&events, "problem_cleared");
    assert_eq!(cleared.len(), 1, "{events:?}");
    assert_eq!(cleared[0]["problem"], format!("halted.{reason}"));
    assert_eq!(cleared[0]["lasted"], "under_1h");
    driver.stop().await;
}

#[tokio::test]
async fn another_folder_with_the_spaces_marker_waits_for_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let original = fs::metadata(&root).unwrap().ino();
    let driver = Driver::open(paths).unwrap();
    driver.start();
    eventually("the folder is claimed", || {
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
    fs::write(root.join("replacement-only.txt"), b"not synced on its own").unwrap();
    assert_ne!(fs::metadata(&root).unwrap().ino(), original);
    eventually("the folder waits for a restart", || {
        driver.status().folders[0]
            .problem
            .as_ref()
            .is_some_and(|problem| problem.contains("koi folder restart"))
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let status = driver.status();
    assert_eq!(status.folders[0].state, FolderState::Halted);
    assert_eq!(claim::owner(&root).unwrap(), None);
    assert!(claim::lock(&root).unwrap().is_some());

    driver.restart(&root).await.unwrap();
    eventually("the restart claims the folder at the path", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;
    driver.stop().await;
}

#[tokio::test]
async fn a_restart_syncs_a_folder_again_once_its_problem_is_fixed() {
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
    let problem = halted(&driver).await;
    assert!(problem.contains("device d2"), "{problem}");

    claim::release(&root, &other).unwrap().durable.unwrap();
    let restarted = driver.restart(&root).await.unwrap();
    assert_ne!(restarted.state, FolderState::Halted);
    eventually("the folder is claimed by this device", || {
        claim::owner(&root)
            .unwrap()
            .is_some_and(|owner| owner.device_id == "d1")
    })
    .await;
    driver.stop().await;
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    use tokio::io::AsyncReadExt;

    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = socket.read(&mut chunk).await.unwrap();
        request.extend_from_slice(&chunk[..n]);
        if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0);
    while request.len() < head_end + length {
        let n = socket.read(&mut chunk).await.unwrap();
        request.extend_from_slice(&chunk[..n]);
    }
    let path = head.split_whitespace().nth(1).unwrap().to_owned();
    (path, request[head_end..head_end + length].to_vec())
}

async fn write_reply(socket: &mut tokio::net::TcpStream, reply: &[u8]) {
    use tokio::io::AsyncWriteExt;

    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
    socket.write_all(reply).await.unwrap();
}

async fn serve_once(reply: Vec<u8>) -> (String, tokio::task::JoinHandle<(String, Vec<u8>)>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        write_reply(&mut socket, &reply).await;
        request
    });
    (server, served)
}

struct Held {
    arrived: tokio::sync::oneshot::Receiver<()>,
    release: tokio::sync::oneshot::Sender<()>,
    served: tokio::task::JoinHandle<Vec<String>>,
}

async fn serve_held(replies: Vec<Vec<u8>>) -> (String, Held) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let (arrived_tx, arrived) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let served = tokio::spawn(async move {
        let mut paths = Vec::new();
        let mut arrived_tx = Some(arrived_tx);
        let mut released = Some(released);
        let last = replies.len() - 1;
        for (i, reply) in replies.into_iter().enumerate() {
            let (mut socket, _) = listener.accept().await.unwrap();
            paths.push(read_request(&mut socket).await.0);
            if i == last {
                arrived_tx.take().unwrap().send(()).unwrap();
                released.take().unwrap().await.unwrap();
            }
            write_reply(&mut socket, &reply).await;
        }
        paths
    });
    (
        server,
        Held {
            arrived,
            release,
            served,
        },
    )
}

async fn still_waiting<T>(task: &tokio::task::JoinHandle<T>) -> bool {
    tokio::time::sleep(Duration::from_millis(200)).await;
    !task.is_finished()
}

/// Sets the mode of `dir` and tells whether it keeps this process out: it does not for root. The tests that rely
/// on it skip when it does not.
fn close_dir(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    let probe = dir.join("probe");
    if fs::write(&probe, b"").is_err() {
        return true;
    }
    fs::remove_file(probe).unwrap();
    open_dir(dir);
    assert!(rustix::process::geteuid().is_root());
    eprintln!("SKIPPED: running as root, so permissions do not block writes");
    false
}

fn open_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
}

fn issued(device_id: &str) -> Vec<u8> {
    prost::Message::encode_to_vec(&DeviceCredentials {
        user_id: "u1".into(),
        device_id: device_id.into(),
        refresh_credential: "secret".into(),
        ..Default::default()
    })
}

#[tokio::test]
async fn claiming_creates_the_first_device_with_its_user_name() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(matches!(
        driver
            .claim("http://127.0.0.1:9/", "code", " ", "laptop")
            .await,
        Err(DriverError::EmptyUserName)
    ));

    let (server, served) = serve_once(issued("d1")).await;
    let (account, _) = driver
        .claim(&server, "claim-code", "Ada", "laptop")
        .await
        .unwrap();
    assert_eq!(account.user_name.as_deref(), Some("Ada"));
    assert_eq!(account.device_id, "d1");
    let (path, body) = served.await.unwrap();
    assert_eq!(path, "/v1/claim");
    let request: ClaimRequest = prost::Message::decode(body.as_slice()).unwrap();
    assert_eq!(
        (
            request.code.as_str(),
            request.user_name.as_str(),
            request.device_name.as_str()
        ),
        ("claim-code", "Ada", "laptop")
    );
    assert!(matches!(
        driver.claim(&server, "claim-code", "Ada", "laptop").await,
        Err(DriverError::AlreadyPaired { .. })
    ));
    drop(driver);
    let reopened = Driver::open(paths).unwrap();
    assert_eq!(reopened.status().account, Some(account));
}

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

#[tokio::test]
async fn a_pause_and_a_resume_during_a_restart_leave_the_folder_one_task() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let driver = Driver::open(paths).unwrap();
    driver.inner.running.store(true, Ordering::SeqCst);
    let (stop, mut stopped) = watch::channel(false);
    let (finished, done) = watch::channel(());
    driver
        .inner
        .folders
        .lock()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .run = Some(Running {
        stop,
        requests: Arc::new(Notify::new()),
        done,
    });
    let (reached, stopped_before_lift) = tokio::sync::oneshot::channel();
    let (proceed, released) = tokio::sync::oneshot::channel();
    *driver.inner.restart_stopped.lock().unwrap() = Some(RestartStopped {
        reached,
        proceed: released,
    });
    let restarting = tokio::spawn({
        let driver = driver.clone();
        let root = root.clone();
        async move {
            driver.restart(&root).await.unwrap();
            driver
                .inner
                .folders
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .run
                .as_ref()
                .unwrap()
                .done
                .clone()
        }
    });
    stopped.changed().await.unwrap();
    assert!(*stopped.borrow());
    drop(finished);
    stopped_before_lift.await.unwrap();
    let changing = tokio::spawn({
        let driver = driver.clone();
        async move {
            assert!(driver.pause().await.unwrap());
            assert!(driver.resume().await.unwrap());
            driver
                .inner
                .folders
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .run
                .as_ref()
                .map(|run| run.done.clone())
        }
    });
    let blocked = still_waiting(&changing).await;
    proceed.send(()).unwrap();
    let restarted = restarting.await.unwrap();
    let resumed = changing.await.unwrap();
    assert!(
        blocked,
        "pause/resume bypassed the stop/lift/start transition"
    );
    let resumed = resumed.unwrap();
    assert!(
        restarted.has_changed().is_err(),
        "pause left the task from restart running"
    );
    assert!(
        resumed.has_changed().is_ok(),
        "resume lost its replacement task"
    );
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;
    let folder = &driver.status().folders[0];
    assert_ne!(folder.state, FolderState::Halted, "{:?}", folder.problem);
    driver.stop().await;
    assert!(resumed.has_changed().is_err());
}

#[tokio::test]
async fn a_resume_after_a_pause_cancelled_before_its_task_ran_stays_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let mut pausing = Box::pin(driver.pause());
    let first = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(pausing.as_mut(), cx))
    })
    .await;
    assert!(first.is_pending());
    drop(pausing);
    driver.resume().await.unwrap();
    tokio::task::yield_now().await;
    assert!(!driver.status().paused);
    assert!(!paths.paused().exists());
}

#[tokio::test]
async fn a_resume_after_a_pause_whose_caller_stopped_waiting_starts_every_folder_again() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let driver = Driver::open(paths).unwrap();
    driver.inner.running.store(true, Ordering::SeqCst);
    let (stop, _stopped) = watch::channel(false);
    let (finished, done) = watch::channel(());
    let stopping = Running {
        stop,
        requests: Arc::new(Notify::new()),
        done,
    };
    driver
        .inner
        .folders
        .lock()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .run = Some(stopping);

    let abandoned = tokio::time::timeout(Duration::from_millis(100), driver.pause()).await;
    assert!(
        abandoned.is_err(),
        "the pause waits for the folder's task to end"
    );
    assert!(driver.status().paused);
    let resuming = tokio::spawn({
        let driver = driver.clone();
        async move { driver.resume().await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(finished);
    assert!(resuming.await.unwrap().unwrap());
    {
        let folders = driver.inner.folders.lock().unwrap();
        let run = folders.values().next().unwrap().run.as_ref();
        assert!(
            run.is_some_and(|run| !run.ended()),
            "a new task syncs the folder"
        );
    }
    driver.stop().await;
}

#[tokio::test]
async fn a_pause_ends_the_wait_of_each_folder_it_stops() {
    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths).unwrap();
    let wait = crate::status::Wait {
        kind: crate::status::WaitKind::Retry,
        until_ms: Some(i64::MAX),
        unreachable: None,
    };
    for folder in driver.inner.folders.lock().unwrap().values() {
        folder.status.lock().unwrap().wait = Some(wait.clone());
    }
    assert_eq!(driver.status().folders[0].wait, Some(wait));
    assert!(driver.pause().await.unwrap());
    let folder = &driver.status().folders[0];
    assert_eq!(
        (folder.state, folder.wait.clone()),
        (FolderState::Stopped, None)
    );
}

#[tokio::test]
async fn the_status_reports_the_saved_update_check_and_off_once_config_toml_turns_it_off() {
    let (_dir, paths, _root) = profile(&[]);
    let release = serde_json::json!({
        "asset": crate::update::this_asset(),
        "checked_at": 7,
        "release": { "version": "999.0.0", "url": "https://example.com/v999.0.0" },
    });
    fs::write(paths.update_check(), release.to_string()).unwrap();
    let driver = release_driver(paths.clone());
    assert_eq!(
        driver.status().update,
        crate::Update::Available {
            latest: "999.0.0".into(),
            url: "https://example.com/v999.0.0".into(),
            checked_at_ms: 7,
        }
    );
    drop(driver);

    fs::write(&paths.config_file, "[updates]\ncheck = false\n").unwrap();
    let driver = release_driver(paths.clone());
    assert_eq!(driver.status().update, crate::Update::Off);
    driver.check_for_updates();
    assert!(!paths.update_check().exists());
}

#[tokio::test]
async fn only_a_file_marks_a_profile_paused() {
    let (_dir, paths, _root) = profile(&[]);
    fs::create_dir_all(paths.paused()).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(!driver.status().paused);
    assert!(!driver.resume().await.unwrap());
    assert!(matches!(driver.pause().await, Err(DriverError::Io { .. })));
    assert!(!driver.status().paused);
}

#[tokio::test]
async fn a_pause_whose_write_fails_with_the_paused_file_in_place_still_pauses() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block writes");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    assert_ne!(driver.status().folders[0].state, FolderState::Stopped);
    fs::write(paths.paused(), b"paused\n").unwrap();
    fs::set_permissions(&paths.state_dir, fs::Permissions::from_mode(0o500)).unwrap();
    let paused = driver.pause().await;
    fs::set_permissions(&paths.state_dir, fs::Permissions::from_mode(0o700)).unwrap();

    let error = paused.unwrap_err();
    assert!(
        matches!(error, DriverError::PausedUnsaved { .. }),
        "{error:?}"
    );
    assert!(
        error.to_string().starts_with("sync is paused, but "),
        "{error}"
    );
    assert!(driver.status().paused);
    assert_eq!(driver.status().folders[0].state, FolderState::Stopped);
}

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

#[tokio::test]
async fn a_pause_of_a_driver_that_was_not_started_only_keeps_the_profile_paused() {
    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(driver.pause().await.unwrap());
    assert!(paths.paused().exists());
    assert!(!driver.status().running);
    assert!(driver.resume().await.unwrap());
    assert_eq!(driver.status().folders[0].state, FolderState::Stopped);
}

#[tokio::test]
async fn watchers_wake_when_status_or_questions_change() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    fs::write(root.join("one"), b"1").unwrap();
    let store = paths.store(SPACE).unwrap();
    Store::open(&store)
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
    let driver = Driver::open(paths).unwrap();
    let mut watch = driver.watch();
    assert!(!watch.has_changed().unwrap());

    driver.answer("q1", Answer::Restore).unwrap();
    assert!(watch.has_changed().unwrap());
    watch.borrow_and_update();

    driver.start();
    assert!(watch.has_changed().unwrap());
    watch.borrow_and_update();
    while driver.status().folders[0].local_entries != Some(1) {
        watch.changed().await.unwrap();
    }

    watch.borrow_and_update();
    driver.stop().await;
    assert!(watch.has_changed().unwrap());
}

#[tokio::test]
async fn progress_alone_wakes_a_watcher_at_most_once_an_interval_and_a_state_change_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let driver = Driver::open(Paths::under(dir.path())).unwrap();
    let mut changes = driver.changes();
    let idle = tokio::time::timeout(Duration::from_secs(1), changes.next()).await;
    assert!(idle.is_err(), "an idle driver wakes no watcher");

    let progressed = driver.inner.progressed.clone();
    let marking = tokio::spawn(async move {
        let started = Instant::now();
        let mut marks = 0;
        while started.elapsed() < Duration::from_secs(1) {
            progressed.send_replace(());
            marks += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        marks
    });
    let started = Instant::now();
    let mut wakes: u128 = 0;
    while started.elapsed() < Duration::from_secs(1) {
        assert_eq!(changes.next().await.unwrap(), Change::Progress);
        wakes += 1;
    }
    let elapsed = started.elapsed();
    let marks = marking.await.unwrap();
    let most = 1 + elapsed.as_millis() / PROGRESS_INTERVAL.as_millis();
    assert!(marks > 100, "{marks} marks");
    assert!(
        (3..=most).contains(&wakes),
        "{wakes} wakes for {marks} marks in {elapsed:?}"
    );

    driver.inner.progressed.send_replace(());
    let changed = driver.inner.changed.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        changed.send_replace(());
    });
    let asked = Instant::now();
    assert_eq!(changes.next().await.unwrap(), Change::State);
    assert!(asked.elapsed() < PROGRESS_INTERVAL, "{:?}", asked.elapsed());
    let after = tokio::time::timeout(Duration::from_secs(1), changes.next()).await;
    assert!(after.is_err(), "the state change took the progress with it");
}

#[tokio::test]
async fn a_progress_change_waiting_out_the_interval_survives_a_dropped_wait() {
    let dir = tempfile::tempdir().unwrap();
    let driver = Driver::open(Paths::under(dir.path())).unwrap();
    let mut changes = driver.changes();
    driver.inner.progressed.send_replace(());
    assert_eq!(changes.next().await.unwrap(), Change::Progress);

    driver.inner.progressed.send_replace(());
    let dropped = tokio::time::timeout(PROGRESS_INTERVAL / 5, changes.next()).await;
    assert!(dropped.is_err(), "the interval holds the change back");
    let kept = tokio::time::timeout(PROGRESS_INTERVAL * 2, changes.next()).await;
    assert_eq!(kept.ok().map(Result::unwrap), Some(Change::Progress));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_reader_opens_the_store_of_a_folder_while_it_is_unlinked() {
    for _ in 0..25 {
        let (_dir, paths, root) = profile(&[]);
        let driver = Driver::open(paths.clone()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let reader = {
            let (driver, stop, reads) = (driver.clone(), stop.clone(), reads.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let _ = driver.open_questions();
                    let _ = driver.answered_question("q1");
                    let _ = driver.answer("q1", Answer::Delete);
                    let _ = driver.restores(SPACE);
                    reads.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        while reads.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        let unlinked = driver.unlink(&root).await;
        stop.store(true, Ordering::SeqCst);
        reader.join().unwrap();
        assert!(unlinked.is_ok(), "{unlinked:?}");
        let store_dir = paths.store(SPACE).unwrap().parent().unwrap().to_path_buf();
        assert!(
            !store_dir.exists(),
            "a reader left {} behind",
            store_dir.display()
        );
    }
}

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

async fn serve_each(
    replies: Vec<(u16, Vec<u8>)>,
    credential: PathBuf,
) -> (String, tokio::task::JoinHandle<Vec<(String, bool)>>) {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (path, _) = read_request(&mut socket).await;
            requests.push((path, credential.exists()));
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

fn access_token() -> (u16, Vec<u8>) {
    let token = koidrive_proto::v1::AccessToken {
        token: "token".into(),
        expires_at_ms: i64::MAX,
        ..Default::default()
    };
    (200, prost::Message::encode_to_vec(&token))
}

fn last_device_refused() -> (u16, Vec<u8>) {
    let refusal = koidrive_proto::v1::Error {
        code: koidrive_proto::v1::error::Code::ConfirmationRequired.into(),
        message: "this is the user's last active device; confirm to revoke it".into(),
        ..Default::default()
    };
    (409, prost::Message::encode_to_vec(&refusal))
}

fn linked_with_server(base: &Path, server: String) -> Paths {
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    fs::remove_file(paths.credential()).unwrap();
    Credential {
        server,
        ..credential("d1")
    }
    .save_new(&paths.credential())
    .unwrap();
    paths
}

#[tokio::test]
async fn a_refused_last_device_keeps_its_credential_and_folders_until_it_confirms() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let credential_path = Paths::under(&base.join("profile")).credential();
    let (server, served) = serve_each(
        vec![access_token(), last_device_refused(), (204, Vec::new())],
        credential_path,
    )
    .await;
    let paths = linked_with_server(&base, server);
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();

    assert!(matches!(
        driver.sign_out(false).await,
        Err(DriverError::LastDevice)
    ));
    let status = driver.status();
    assert_eq!(status.account.unwrap().device_id, "d1");
    assert_eq!(status.folders.len(), 1);
    assert!(status.running);
    assert!(paths.credential().exists());

    assert_eq!(driver.sign_out(true).await.unwrap().unconfirmed, None);
    let status = driver.status();
    assert_eq!(status.account, None);
    assert!(status.folders.is_empty());
    assert!(!paths.credential().exists());
    assert_eq!(
        served.await.unwrap(),
        [
            ("/v1/token".to_owned(), true),
            ("/v1/devices/d1".to_owned(), true),
            ("/v1/devices/d1?confirm=last-device".to_owned(), true),
        ]
    );
}

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

async fn serve_calling(
    replies: Vec<(u16, Vec<u8>)>,
    at: usize,
    call: impl FnOnce() + Send + 'static,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let mut call = Some(call);
        let mut requests = Vec::new();
        for (i, (status, body)) in replies.into_iter().enumerate() {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await.0);
            if i == at {
                call.take().unwrap()();
            }
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

async fn paused_driver(paths: &Paths, root: &Path) -> (Driver, Gate) {
    linked(paths, root, "d1");
    fs::write(root.join("file"), b"mine").unwrap();
    let mut driver = Driver::open(paths.clone()).unwrap();
    let (entered, observing) = tokio::sync::oneshot::channel();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    Arc::get_mut(&mut driver.inner)
        .unwrap()
        .rules
        .get_mut()
        .unwrap()
        .matcher = Arc::new(Paused {
        entered: Mutex::new(Some(entered)),
        gate: gate.clone(),
    });
    driver.start();
    observing.await.unwrap();
    (driver, Gate(gate))
}

fn lockable(root: &Path) -> bool {
    claim::lock(root).unwrap().is_some()
}

fn signed_out(driver: &Driver, paths: &Paths, root: &Path) {
    let status = driver.status();
    assert_eq!(status.account, None);
    assert!(status.folders.is_empty());
    assert!(!status.running);
    assert!(!paths.credential().exists());
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    assert_eq!(claim::owner(root).unwrap(), None);
    assert!(lockable(root));
    assert_eq!(fs::read(root.join("file")).unwrap(), b"mine");
}

#[tokio::test]
async fn signing_out_waits_for_a_stop_already_in_progress() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    let stop = tokio::spawn({
        let driver = driver.clone();
        async move { driver.stop().await }
    });
    eventually("the stop begins", || !driver.status().running).await;
    let sign_out = tokio::spawn({
        let driver = driver.clone();
        async move { driver.sign_out(true).await }
    });
    assert!(still_waiting(&sign_out).await);
    assert!(!lockable(&root));

    gate.open();
    stop.await.unwrap();
    sign_out.await.unwrap().unwrap();
    signed_out(&driver, &paths, &root);
}

fn uploading(driver: &Driver, size: u64) -> Arc<crate::lifecycle::Lifecycle> {
    let folders = driver.inner.folders.lock().unwrap();
    let folder = folders.values().next().unwrap();
    let item = crate::progress::Item {
        direction: crate::progress::Direction::Upload,
        node: "a".into(),
        digest: b"a".to_vec(),
    };
    (folder.progress.lock().unwrap()).track(BTreeMap::from([(item, size)]), |_| false);
    folder.status.clone()
}

#[tokio::test]
async fn a_paused_driver_reports_no_progress() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    let status = uploading(&driver, 100);
    status.lock().unwrap().state = FolderState::CatchingUp;
    assert_eq!(
        driver.status().progress.map(|totals| totals.bytes_total),
        Some(100)
    );

    let pause = tokio::spawn({
        let driver = driver.clone();
        async move { driver.pause().await }
    });
    gate.open();
    assert!(pause.await.unwrap().unwrap());
    let paused = driver.status();
    assert_eq!(paused.folders[0].progress, None);
    assert_eq!(paused.progress, None);
}

#[tokio::test]
async fn the_status_shows_the_files_the_first_scan_has_found_so_far() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    let (status, found) = {
        let folders = driver.inner.folders.lock().unwrap();
        let folder = folders.values().next().unwrap();
        (folder.status.clone(), folder.observing.clone())
    };
    {
        let mut shown = status.lock().unwrap();
        shown.state = FolderState::CatchingUp;
        shown.activity = Some(Activity::Scanning { files: 0 });
    }
    *found.lock().unwrap() = Some(crate::observe::Observing::Walking { files: 12_345 });
    assert_eq!(
        driver.status().folders[0].activity,
        Some(Activity::Scanning { files: 12_345 })
    );
    *found.lock().unwrap() = Some(crate::observe::Observing::Reading {
        files: 7,
        bytes: 9_000,
    });
    assert_eq!(
        driver.status().folders[0].activity,
        Some(Activity::Checking {
            files: 7,
            bytes: 9_000
        })
    );
    {
        let mut shown = status.lock().unwrap();
        shown.progress = Some(crate::status::Progress::default());
        shown.activity = None;
        assert!(shown.end_transfers());
        assert!(!shown.end_transfers());
    }
    *found.lock().unwrap() = None;
    assert_eq!(
        driver.status().folders[0].activity,
        Some(Activity::CatchingUp)
    );
    status.lock().unwrap().state = FolderState::Offline;
    assert_eq!(driver.status().folders[0].activity, None);

    let tracker = driver
        .inner
        .folders
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .progress
        .clone();
    crate::progress::Credit::download(&tracker, []).moved(4_096);
    assert_eq!(driver.status().folders[0].bytes_moved, 4_096);
    gate.open();
    driver.stop().await;
}

#[tokio::test]
async fn signing_out_takes_its_folders_out_of_the_progress() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    uploading(&driver, 100);
    assert!(driver.inner.totals.lock().unwrap().totals().is_some());

    let sign_out = tokio::spawn({
        let driver = driver.clone();
        async move { driver.sign_out(true).await }
    });
    gate.open();
    sign_out.await.unwrap().unwrap();
    signed_out(&driver, &paths, &root);
    assert_eq!(driver.inner.totals.lock().unwrap().totals(), None);
}

#[tokio::test]
async fn unlinking_takes_the_folder_out_of_the_progress() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    uploading(&driver, 100);

    let unlink = tokio::spawn({
        let driver = driver.clone();
        let root = root.clone();
        async move { driver.unlink(&root).await }
    });
    gate.open();
    unlink.await.unwrap().unwrap();
    assert_eq!(driver.inner.totals.lock().unwrap().totals(), None);
}

#[tokio::test]
async fn signing_out_finishes_a_cancelled_unlink() {
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
    assert_eq!(driver.status().folders.len(), 1);
    let sign_out = tokio::spawn({
        let driver = driver.clone();
        async move { driver.sign_out(true).await }
    });
    assert!(still_waiting(&sign_out).await);

    gate.open();
    sign_out.await.unwrap().unwrap();
    signed_out(&driver, &paths, &root);
}

#[tokio::test]
async fn linking_or_relinking_a_folder_whose_unlink_is_unfinished_is_refused() {
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
    let space_id = driver.status().folders[0].space_id.clone();
    let linked = driver.link(&root, &space_id).await;
    assert!(
        matches!(linked, Err(DriverError::UnlinkUnfinished(ref folder)) if *folder == root),
        "{linked:?}"
    );
    let relinked = driver.relink(&root).await;
    assert!(
        matches!(relinked, Err(DriverError::UnlinkUnfinished(ref folder)) if *folder == root),
        "{relinked:?}"
    );
    driver.sign_out(true).await.unwrap();
    signed_out(&driver, &paths, &root);
}

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
    OpenOptions::new()
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

fn server_id(n: u128) -> String {
    koidrive_engine::ids::uuid_v7(1, n)
}

fn from_trash(node: u128) -> RestoreRequest {
    RestoreRequest::FromTrash {
        node_id: server_id(node),
        parent_id: None,
        name: None,
    }
}

#[test]
fn a_restore_request_survives_a_reopen_and_reads_as_pending() {
    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths.clone()).unwrap();
    let mut watch = driver.watch();
    watch.mark_unchanged();
    let placed = RestoreRequest::FromTrash {
        node_id: server_id(1),
        parent_id: Some(server_id(2)),
        name: Some("back.txt".into()),
    };
    let version = RestoreRequest::Version {
        node_id: server_id(3),
        version_id: server_id(4),
        saved_at_ms: Some(1_794_000_000_000),
    };
    assert_eq!(driver.restores(SPACE).unwrap(), []);
    let first = driver
        .restore(SPACE, placed.clone(), Seen::default())
        .unwrap();
    assert!(watch.has_changed().unwrap());
    std::thread::sleep(Duration::from_millis(2));
    let second = driver
        .restore(SPACE, version.clone(), Seen::default())
        .unwrap();
    assert!(first < second, "the IDs sort in request order");
    drop(driver);

    let driver = Driver::open(paths).unwrap();
    let pending = |id: &str, request: &RestoreRequest| RestoreStatus {
        id: id.to_owned(),
        path: None,
        asked_at_ms: koidrive_engine::ids::unix_ms(id).unwrap() as i64,
        request: request.clone(),
        state: RestoreState::Pending { waits_for: None },
    };
    assert_eq!(
        driver.restores(SPACE).unwrap(),
        [pending(&first, &placed), pending(&second, &version)]
    );
}

#[test]
fn restore_refuses_a_malformed_id_and_an_unlinked_space() {
    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths).unwrap();
    let other_space = "0190f0a0-0000-7000-8000-000000000002";
    assert!(matches!(
        driver.restore(other_space, from_trash(1), Seen::default()),
        Err(DriverError::SpaceNotLinked(space)) if space == other_space
    ));
    assert!(matches!(
        driver.restores(other_space),
        Err(DriverError::SpaceNotLinked(_))
    ));
    let version = |node_id: &str, version_id: &str| RestoreRequest::Version {
        node_id: node_id.into(),
        version_id: version_id.into(),
        saved_at_ms: None,
    };
    let placed = |parent_id: &str| RestoreRequest::FromTrash {
        node_id: server_id(1),
        parent_id: Some(parent_id.into()),
        name: None,
    };
    let cases = [
        (version("report.txt", &server_id(2)), "node_id"),
        (version(&server_id(1), ""), "version_id"),
        (version(&server_id(1), "v2"), "version_id"),
        (
            placed("0190F3A0-1234-7FFF-BFFF-FFFFFFFFFFFF"),
            "target_parent_id",
        ),
    ];
    for (request, field) in cases {
        assert!(matches!(
            driver.restore(SPACE, request, Seen::default()),
            Err(DriverError::InvalidRestore(found)) if found == field
        ));
    }
    assert_eq!(
        driver.restores(SPACE).unwrap(),
        [],
        "a commit with a malformed ID would halt the space, so none is recorded"
    );
}

#[test]
fn restores_lists_what_ended_and_then_what_waits() {
    use koidrive_engine::restore::{Outcome, Rejection, Resolved, RestoreIntent};

    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths.clone()).unwrap();
    let in_inbox = driver
        .restore(SPACE, from_trash(9), Seen::default())
        .unwrap();
    let intent = |node: u128| RestoreIntent::from_trash(server_id(node), None, None).unwrap();
    let outcomes = [
        (Outcome::Restored, RestoreState::Restored),
        (
            Outcome::Renamed { name: "b".into() },
            RestoreState::Renamed { name: "b".into() },
        ),
        (
            Outcome::Copied {
                node: server_id(50),
                name: "c".into(),
            },
            RestoreState::Copied {
                node_id: server_id(50),
                name: "c".into(),
            },
        ),
        (
            Outcome::Rejected(Rejection::NotFound),
            RestoreState::Rejected {
                reason: RestoreRejection::NotFound,
            },
        ),
        (
            Outcome::Rejected(Rejection::ReadOnly),
            RestoreState::Rejected {
                reason: RestoreRejection::ReadOnly,
            },
        ),
        (
            Outcome::Rejected(Rejection::Other("LATER".into())),
            RestoreState::Rejected {
                reason: RestoreRejection::Other("LATER".into()),
            },
        ),
        (Outcome::DoesNotFit, RestoreState::DoesNotFit),
    ];
    let mut state = koidrive_store::EngineState::default();
    state.commits.restores.insert("queued".into(), intent(8));
    for (n, (outcome, _)) in outcomes.iter().enumerate() {
        state.commits.resolved.push(Resolved {
            id: format!("ended-{}", 9 - n),
            intent: intent(n as u128),
            outcome: outcome.clone(),
        });
    }
    let path = paths.store(SPACE).unwrap();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();

    let listed = driver.restores(SPACE).unwrap();
    let ids: Vec<&str> = listed.iter().map(|restore| restore.id.as_str()).collect();
    let ended = (0..outcomes.len()).map(|n| format!("ended-{}", 9 - n));
    let expected: Vec<String> = ended.chain([in_inbox, "queued".into()]).collect();
    assert_eq!(ids, expected);
    for (restore, (_, state)) in listed.iter().zip(&outcomes) {
        assert_eq!(&restore.state, state);
    }
    assert_eq!(listed[1].request, from_trash(1));
    for waiting in &listed[outcomes.len()..] {
        assert_eq!(waiting.state, RestoreState::Pending { waits_for: None });
    }
}

#[test]
fn restores_name_the_path_of_their_node_and_when_they_were_asked_for() {
    use koidrive_proto::v1::{Content, NodeKind, NodeState, NodeStatus, VersionInfo};

    let (_dir, paths, _root) = profile(&[]);
    let node = |id: u128, parent: &str, name: &str, kind: NodeKind, status: NodeStatus| NodeState {
        node_id: server_id(id),
        parent_id: parent.to_owned(),
        name: name.to_owned(),
        kind: kind as i32,
        status: status as i32,
        trash_group: match status {
            NodeStatus::Trashed => "group".into(),
            _ => String::new(),
        },
        current: Some(VersionInfo {
            version_id: server_id(100 + id),
            content: Some(Content::default()),
            ..VersionInfo::default()
        }),
        ..NodeState::default()
    };
    let docs = server_id(1);
    let nodes = [
        node(1, SPACE, "Docs", NodeKind::Dir, NodeStatus::Live),
        node(2, &docs, "report.docx", NodeKind::File, NodeStatus::Live),
        node(3, &docs, "old.pptx", NodeKind::File, NodeStatus::Trashed),
        node(
            4,
            &server_id(5),
            "orphan.txt",
            NodeKind::File,
            NodeStatus::Trashed,
        ),
    ];
    Store::open(paths.store(SPACE).unwrap())
        .unwrap()
        .transaction(|tx| nodes.iter().try_for_each(|node| tx.put_remote_node(node)))
        .unwrap();
    let driver = Driver::open(paths).unwrap();
    let before = now_ms();
    let version = RestoreRequest::Version {
        node_id: server_id(2),
        version_id: server_id(200),
        saved_at_ms: None,
    };
    let asked: Vec<String> = [
        (version, Some("Earlier/report.docx")),
        (from_trash(3), None),
        (from_trash(4), Some("Before the first sync/orphan.txt")),
        (from_trash(9), None),
    ]
    .into_iter()
    .map(|(request, path)| {
        driver
            .restore(
                SPACE,
                request,
                Seen {
                    path: path.map(str::to_owned),
                    ..Seen::default()
                },
            )
            .unwrap()
    })
    .collect();
    let after = now_ms();

    let listed = driver.restores(SPACE).unwrap();
    let found: Vec<Option<&str>> = (asked.iter())
        .map(|id| {
            let restore = listed.iter().find(|restore| &restore.id == id).unwrap();
            restore.path.as_ref().map(koidrive_fs::ItemPath::unquoted)
        })
        .collect();
    assert_eq!(
        found,
        [
            Some("Docs/report.docx"),
            Some("Docs/old.pptx"),
            Some("Before the first sync/orphan.txt"),
            None
        ]
    );
    for restore in &listed {
        assert!(
            (before..=after).contains(&restore.asked_at_ms),
            "{restore:?} was asked for between {before} and {after}"
        );
    }
}

#[tokio::test]
async fn a_restore_asked_for_while_the_folder_runs_reaches_its_engine() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    eventually("the folder's task runs its loop", || {
        driver.status().folders[0].state == FolderState::Offline
    })
    .await;

    let id = driver
        .restore(SPACE, from_trash(1), Seen::default())
        .unwrap();
    let path = paths.store(SPACE).unwrap();
    eventually("the engine holds the request", || {
        let (inbox, (queued, _)) = Store::open(&path)
            .unwrap()
            .transaction(|tx| {
                Ok::<_, koidrive_store::Error>((tx.restore_requests()?, tx.restores()?))
            })
            .unwrap();
        inbox.is_empty() && queued.contains_key(&id)
    })
    .await;
    let listed = driver.restores(SPACE).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, RestoreState::Pending { waits_for: None });
    driver.stop().await;
}

#[tokio::test]
async fn a_paused_folder_no_longer_says_it_cannot_reach_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    eventually("the folder shows that it cannot reach the server", || {
        let folder = &driver.status().folders[0];
        folder.unreachable().is_some()
    })
    .await;
    assert!(driver.pause().await.unwrap());
    let folder = driver.status().folders[0].clone();
    assert_eq!(folder.state, FolderState::Stopped);
    assert_eq!(folder.wait, None, "paused: {folder:?}");
    driver.stop().await;
}

#[test]
fn a_restore_held_by_a_problem_node_reads_as_pending_with_the_reason() {
    use koidrive_engine::restore::RestoreIntent;

    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths.clone()).unwrap();
    let mut state = koidrive_store::EngineState::default();
    state.commits.problems.insert(server_id(1));
    let held = RestoreIntent::version(server_id(1), server_id(40)).unwrap();
    let free = RestoreIntent::from_trash(server_id(2), None, None).unwrap();
    state.commits.restores.insert("a-held".into(), held);
    state.commits.restores.insert("b-free".into(), free);
    let path = paths.store(SPACE).unwrap();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();
    let asked = driver
        .restore(SPACE, from_trash(1), Seen::default())
        .unwrap();

    let listed = driver.restores(SPACE).unwrap();
    let states: Vec<_> = listed.iter().map(|r| (r.id.as_str(), &r.state)).collect();
    let problem = RestoreState::Pending {
        waits_for: Some(RestoreWait::NodeProblem),
    };
    let free = RestoreState::Pending { waits_for: None };
    assert_eq!(
        states,
        [
            (asked.as_str(), &problem),
            ("a-held", &problem),
            ("b-free", &free)
        ]
    );
    assert_eq!(
        serde_json::to_value(&problem).unwrap(),
        serde_json::json!({ "type": "pending", "waits_for": "node_problem" })
    );
}

#[test]
fn a_question_stored_without_its_limits_names_them() {
    let decoded = |deletions, affected_files, space_files| {
        let question = Question {
            recovery_cause: None,
            id: "q1".into(),
            space_id: SPACE.into(),
            asked_at_ms: 1,
            kind: QuestionKind::MassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded: Vec::new(),
            },
            items: Vec::new(),
            lineage: "q1".into(),
        };
        let body = serde_json::to_string(&question).unwrap();
        let stored = StoredQuestion {
            question_id: "q1".into(),
            asked_at_ms: 1,
            body: body.replace(r#","exceeded":[]"#, "").into_bytes(),
            answer: None,
        };
        match decode_question(SPACE, &stored).unwrap().kind {
            QuestionKind::MassDelete { exceeded, .. } => exceeded,
            kind => panic!("a mass-delete question: {kind:?}"),
        }
    };
    assert_eq!(
        decoded(1_200, 1_500, 2_000),
        [Limit::Deletions, Limit::Share]
    );
    assert_eq!(decoded(3, 30, 40), [Limit::Share]);
    assert_eq!(decoded(3, 3, 40), [Limit::Deletions]);
}

#[tokio::test]
async fn a_gitignore_choice_whose_sync_fails_after_publication_is_what_a_reopen_reads() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    fs::write(&paths.config_file, "[sync]\n").unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(driver.follows_gitignore());
    let failing = koidrive_state_file::fail_syncs_after_publication();
    let set = driver.set_gitignore(false).await;
    drop(failing);
    assert!(set.is_ok());
    assert!(!driver.follows_gitignore());
    drop(driver);
    assert!(!Driver::open(paths).unwrap().follows_gitignore());
}

#[tokio::test]
async fn setting_gitignore_keeps_the_rest_of_the_configuration_file() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let text = "# mine\n[sync]\nexclude = [\"*.iso\"] # big files\n\n[upload]\nconcurrency = 3\n";
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    fs::write(&paths.config_file, text).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(driver.follows_gitignore());

    driver.set_gitignore(false).await.unwrap();
    assert!(!driver.follows_gitignore());
    let off = "# mine\n[sync]\nexclude = [\"*.iso\"] # big files\ngitignore = false\n\n[upload]\nconcurrency = 3\n";
    assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), off);
    driver.set_gitignore(false).await.unwrap();
    assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), off);
    drop(driver);
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(
        !driver.follows_gitignore(),
        "the next start applies the file"
    );

    driver.set_gitignore(true).await.unwrap();
    assert_eq!(
        fs::read_to_string(&paths.config_file).unwrap(),
        off.replace("gitignore = false", "gitignore = true")
    );
    let config = Config::load(&paths.config_file).unwrap();
    assert!(config.sync.gitignore);
    assert_eq!(config.sync.exclude, ["*.iso"]);
    assert_eq!(config.upload.concurrency.get(), 3);

    let fresh = Paths::under(&base.join("fresh"));
    let driver = Driver::open(fresh.clone()).unwrap();
    driver.set_gitignore(false).await.unwrap();
    assert_eq!(
        fs::read_to_string(&fresh.config_file).unwrap(),
        "[sync]\ngitignore = false\n"
    );
}

#[tokio::test]
async fn setting_gitignore_restarts_each_running_folder_once() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    let driver = Driver::open(paths).unwrap();
    driver.start();
    let done = |driver: &Driver| {
        let folders = driver.inner.folders.lock().unwrap();
        folders[SPACE].run.as_ref().unwrap().done.clone()
    };
    let first = done(&driver);

    driver.set_gitignore(false).await.unwrap();
    assert!(first.has_changed().is_err(), "the task that ran has ended");
    let second = done(&driver);
    assert!(second.has_changed().is_ok(), "a new task runs");

    driver.set_gitignore(false).await.unwrap();
    assert!(
        second.has_changed().is_ok(),
        "the same value restarts nothing"
    );
    driver.stop().await;
}

#[tokio::test]
async fn gitignore_only_counts_the_files_only_gitignore_files_leave_out() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    fs::write(&paths.config_file, "[sync]\nexclude = [\"vendor/\"]\n").unwrap();
    for (path, text) in [
        (".gitignore", "node_modules/\n*.log\nvendor/\n"),
        (".koiignore", "secret.log\n!keep.log\n"),
        ("node_modules/a.js", "abc"),
        ("node_modules/pkg/b.js", "12345"),
        ("node_modules/pkg/.koiignore", "c.js\n"),
        ("node_modules/pkg/c.js", "left out by .koiignore"),
        ("x.log", "12"),
        ("secret.log", "left out by .koiignore"),
        ("keep.log", "kept by .koiignore"),
        ("vendor/v.log", "left out by the device"),
        ("notes.txt", "synced"),
        ("sub/.gitignore", "*.tmp\n"),
        ("sub/t.tmp", "tmp"),
        (".DS_Store", "built in"),
        (".git/HEAD", "ref: refs/heads/main"),
        (".git/logs/x.log", "left out with every .git folder"),
        (
            "sub/.git/objects/pack/p.pack",
            "left out with every .git folder",
        ),
    ] {
        fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        fs::write(root.join(path), text).unwrap();
    }
    std::os::unix::fs::symlink(root.join("notes.txt"), root.join("link.log")).unwrap();
    let driver = Driver::open(paths).unwrap();
    let only = GitignoreOnly {
        files: 5,
        bytes: 18,
        unreadable: Vec::new(),
    };
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), only);
    driver.set_gitignore(false).await.unwrap();
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), only);
    assert!(matches!(
        driver.gitignore_only("0190f0a0-0000-7000-8000-000000000002"),
        Err(DriverError::SpaceNotLinked(_))
    ));
}

#[tokio::test]
async fn gitignore_only_counts_a_linked_root_it_cannot_open_as_unreadable() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    linked(&paths, &root, "d1");
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(root.join("a.log"), "abc").unwrap();
    let driver = Driver::open(paths).unwrap();
    let blocked = GitignoreOnly {
        files: 0,
        bytes: 0,
        unreadable: vec![root.clone()],
    };

    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
    let counted = driver.gitignore_only(SPACE);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(counted.unwrap(), blocked);

    fs::rename(&root, base.join("Moved")).unwrap();
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), blocked);
}

#[tokio::test]
async fn setting_gitignore_refuses_a_configuration_file_the_next_start_would_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    for text in [
        "[sync]\nexclude = [\"!keep\"]\n",
        "[sync]\nexclude = [\"!keep\"]\ngitignore = false\n",
    ] {
        fs::write(&paths.config_file, text).unwrap();
        assert!(Config::load(&paths.config_file).is_err());
        assert!(matches!(
            driver.set_gitignore(false).await,
            Err(DriverError::Config(ConfigError::Invalid { .. }))
        ));
        assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), text);
    }
    assert!(driver.follows_gitignore());
}

fn gitignore_only_of(base: &Path, root: &Path) -> Driver {
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, root, "d1");
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(root.join("a.log"), "abc").unwrap();
    Driver::open(paths).unwrap()
}

#[tokio::test]
async fn gitignore_only_does_not_count_a_folder_made_where_the_linked_root_was() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let driver = gitignore_only_of(&base, &root);
    fs::rename(&root, base.join("Moved")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(root.join("b.log"), "12345").unwrap();
    let blocked = GitignoreOnly {
        files: 0,
        bytes: 0,
        unreadable: vec![root],
    };
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), blocked);
}

#[tokio::test]
async fn gitignore_only_does_not_count_the_folder_a_symbolic_link_puts_above_a_linked_root() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Parent/Koidrive");
    let driver = gitignore_only_of(&base, &root);
    fs::rename(base.join("Parent"), base.join("Moved")).unwrap();
    let other = base.join("Other/Koidrive");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join(".gitignore"), "*.log\n").unwrap();
    fs::write(other.join("b.log"), "12345").unwrap();
    std::os::unix::fs::symlink(base.join("Other"), base.join("Parent")).unwrap();
    let blocked = GitignoreOnly {
        files: 0,
        bytes: 0,
        unreadable: vec![root],
    };
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), blocked);
}

#[tokio::test]
async fn gitignore_only_counts_a_linked_root_a_symbolic_link_leads_to() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let driver = gitignore_only_of(&base, &root);
    let moved = base.join("Documents/Koidrive");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(&root, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &root).unwrap();
    let counted = GitignoreOnly {
        files: 1,
        bytes: 3,
        unreadable: Vec::new(),
    };
    assert_eq!(driver.gitignore_only(SPACE).unwrap(), counted);
}
