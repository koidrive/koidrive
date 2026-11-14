// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::sync::Mutex;
use std::time::Duration;

use koidrive_driver::{
    Answered, Change, Driver, DriverError, FolderState, FolderStatus, Linked, Paths, Question,
    Status, Update,
};
use koidrive_engine::commit::Lift;
use koidrive_ipc::{Blocked, Key, Request, RequestId, Response, read_line, write_json, write_line};
use rustix::io::Errno;
use tokio::io::AsyncReadExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};

use crate::error::CliError;
use crate::request::{self, Begun};

impl From<CliError> for Response {
    fn from(error: CliError) -> Self {
        Response::Error {
            message: error.message,
            code: error.code,
            reason: error.reason,
            next: error.next,
        }
    }
}

async fn once(driver: &Driver, request_id: RequestId, key: Key, request: Request) -> Response {
    let mut inner = &request;
    while let Request::Once { run, .. } = inner {
        inner = run;
    }
    if matches!(
        inner,
        Request::Share { .. } | Request::ShareLinks | Request::RevokeShareLink { .. }
    ) {
        return Response::from(CliError::usage(
            "share-link requests cannot use a request ID because their responses contain bearer URLs",
        ));
    }
    let claim = match request::begin(driver.paths(), &request_id, &key) {
        Ok(Begun::Claimed(claim)) => claim,
        Ok(Begun::Done(recorded)) => {
            return serde_json::from_value(recorded).unwrap_or_else(|error| {
                Response::from(CliError::from(format!(
                    "the record of the request {request_id} cannot be read: {error}"
                )))
            });
        }
        Err(error) => return Response::from(error),
    };
    let response = Box::pin(handle(driver, request)).await;
    let failed = match &response {
        Response::Error { .. } => true,
        Response::Restarted { folder } => folder.state == FolderState::Halted,
        _ => false,
    };
    if failed {
        claim.abandon();
    } else if let Err(error) = claim.finish(&response) {
        tracing::warn!(%request_id, error = error.message, "could not record the outcome of a request");
    }
    response
}

pub async fn handle(driver: &Driver, request: Request) -> Response {
    let outcome: Result<Response, DriverError> = match request {
        Request::Status => Ok(Response::Status {
            status: driver.status(),
        }),
        Request::Pair { server, code, name } => match driver.pair(&server, &code, &name).await {
            Ok(account) => Ok(Response::Paired { account }),
            Err(error) => return Response::from(CliError::code_refused(error)),
        },
        Request::Claim {
            server,
            code,
            user_name,
            name,
        } => match driver.claim(&server, &code, &user_name, &name).await {
            Ok(account) => Ok(Response::Paired { account }),
            Err(error) => return Response::from(CliError::code_refused(error)),
        },
        Request::SaveCredential { credential } => driver
            .save_credential(credential)
            .await
            .map(|account| Response::Paired { account }),
        Request::SignOut { device_id } => driver
            .sign_out_device(&device_id)
            .await
            .map(|not_removed| Response::SignedOut { not_removed }),
        Request::Leave {
            confirm_last_device,
        } => driver
            .sign_out(confirm_last_device)
            .await
            .map(|not_removed| Response::SignedOut { not_removed }),
        Request::Link { folder, space } => {
            driver
                .link(&folder, &space)
                .await
                .map(|linked| match linked {
                    Linked::New(folder) => Response::Linked { folder },
                    Linked::Already(folder) => Response::AlreadyLinked { folder },
                })
        }
        Request::LinkApproved {
            folder,
            space,
            approval,
        } => driver
            .link_approved(&folder, &space, Some(approval))
            .await
            .map(|linked| match linked {
                Linked::New(folder) => Response::Linked { folder },
                Linked::Already(folder) => Response::AlreadyLinked { folder },
            }),
        Request::Unlink { folder } => match driver.unlink(&folder).await {
            Ok(link) => Ok(Response::Unlinked {
                space_id: link.space_id,
                path: link.root,
            }),
            Err(DriverError::NotLinked(path)) => Ok(Response::NotLinked { path }),
            Err(error) => Err(error),
        },
        Request::Restart { folder } => driver
            .restart(&folder)
            .await
            .map(|folder| Response::Restarted { folder }),
        Request::Lift { folder, replan } => {
            let lift = if replan { Lift::Replan } else { Lift::Resend };
            (driver.lift(&folder, lift).await).map(|folder| Response::Restarted { folder })
        }
        Request::Questions => driver
            .questions()
            .map(|questions| Response::Questions { questions }),
        Request::AnsweredQuestion { question_id } => driver
            .answered_question(&question_id)
            .map(|question| Response::AnsweredQuestion { question }),
        Request::Answer {
            question_id,
            answer,
        } => driver
            .answer(&question_id, answer)
            .map(|answered| match answered {
                Answered::Now => Response::Answered,
                Answered::Already => Response::AlreadyAnswered,
            }),
        Request::Restore {
            space_id,
            restore,
            folder,
            path,
        } => driver
            .restore(&space_id, restore, Some(&path))
            .map(|id| Response::Restore {
                id,
                space_id,
                folder,
                path,
            }),
        Request::Restores { space_id } => driver
            .restores(&space_id)
            .map(|restores| Response::Restores { restores }),
        Request::Access => Ok(Response::Access {
            blocked: unreadable(driver.status().folders).await,
        }),
        Request::Pause => driver.pause().await.map(|paused| match paused {
            true => Response::Paused,
            false => Response::AlreadyPaused,
        }),
        Request::Resume => driver.resume().await.map(|resumed| match resumed {
            true => Response::Resumed,
            false => Response::NotPaused,
        }),
        Request::Watch => {
            return Response::from(CliError::usage(
                "only a running daemon can be watched, over a connection of its own",
            ));
        }
        Request::Share {
            path,
            expiry,
            password,
        } => (driver.share(&path, expiry, password).await).map(|link| Response::Shared { link }),
        Request::ShareLinks => {
            (driver.share_links().await).map(|links| Response::ShareLinks { links })
        }
        Request::RevokeShareLink { link } => {
            (driver.revoke_share_link(&link).await).map(|revoked| Response::ShareLinkRevoked {
                link: revoked.link,
                already: revoked.already,
            })
        }
        Request::Once {
            request_id,
            key,
            run,
        } => return once(driver, request_id, key, *run).await,
    };
    outcome.unwrap_or_else(|error| Response::from(CliError::from(error)))
}

static READING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// The linked folders this process may not read. macOS keeps a launch agent out of the folders that its privacy
/// settings guard, such as Documents, until the person allows it, and asks while the first read waits. Each read
/// runs on a thread of its own, which a read waiting for the person keeps until the answer, so the runtime never waits
/// for it. A folder whose earlier read still waits gets no second thread, however often the status is asked for.
async fn unreadable(folders: Vec<FolderStatus>) -> Vec<Blocked> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let reads: Vec<_> = (folders.into_iter())
        .map(|folder| {
            let (send, receive) = tokio::sync::oneshot::channel();
            if READING.lock().unwrap().insert(folder.path.clone()) {
                let path = folder.path.clone();
                std::thread::spawn(move || {
                    let read = fs::read_dir(&path).map(drop);
                    READING.lock().unwrap().remove(&path);
                    send.send(read)
                });
            }
            (folder.path, receive)
        })
        .collect();
    let mut blocked = Vec::new();
    for (folder, read) in reads {
        match tokio::time::timeout_at(deadline, read).await {
            Err(_) | Ok(Err(_)) => blocked.push(Blocked {
                folder,
                waiting: true,
            }),
            Ok(Ok(Err(error))) if error.raw_os_error() == Some(Errno::PERM.raw_os_error()) => {
                blocked.push(Blocked {
                    folder,
                    waiting: false,
                })
            }
            Ok(Ok(_)) => {}
        }
    }
    blocked
}

pub async fn serve(listener: UnixListener, driver: Driver) {
    let uid = rustix::process::getuid().as_raw();
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!(%error, "accepting a connection on the socket failed");
                continue;
            }
        };
        match stream.peer_cred() {
            Ok(peer) if peer.uid() == uid => {}
            peer => {
                tracing::warn!(?peer, "refused a connection from another user");
                continue;
            }
        }
        let driver = driver.clone();
        tokio::spawn(async move {
            if let Err(error) = answer(stream, &driver).await {
                tracing::debug!(%error, "a socket connection failed");
            }
        });
    }
}

async fn answer(stream: UnixStream, driver: &Driver) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let line = read_line(&mut reader).await?;
    let response = match serde_json::from_str::<Request>(&line) {
        Ok(Request::Watch) => return watch(reader, writer, driver).await,
        Ok(request) => handle(driver, request).await,
        Err(error) => Response::from(CliError::usage(format!(
            "the request is not valid: {error}"
        ))),
    };
    write_line(&mut writer, &response).await
}

async fn watch(
    mut reader: OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    driver: &Driver,
) -> io::Result<()> {
    let mut changes = driver.changes();
    let mut closed = [0; 1];
    let mut asked: Option<Vec<Question>> = None;
    let mut change = Change::State;
    loop {
        let questions = match asked.take().filter(|_| change == Change::Progress) {
            Some(questions) => Ok(questions),
            None => driver.questions(),
        };
        let state = match questions {
            Ok(questions) => {
                asked = Some(questions.clone());
                Response::State {
                    status: driver.status(),
                    questions,
                }
            }
            Err(error) => Response::from(CliError::from(error)),
        };
        write_json(&mut writer, &state).await?;
        change = tokio::select! {
            change = changes.next() => change.map_err(io::Error::other)?,
            _ = reader.read(&mut closed) => return Ok(()),
        };
    }
}

pub fn busy(paths: &Paths) -> String {
    format!(
        "a Koidrive process holds this profile, but its socket {} does not answer",
        paths.socket().display()
    )
}

pub async fn run(paths: &Paths, request: Request) -> Result<Response, CliError> {
    let response = match koidrive_ipc::ask(paths, &request).await? {
        Some(response) => response,
        None if matches!(request, Request::Status) && !paths.state_dir.exists() => {
            let update = match koidrive_driver::update::is_on(paths) {
                true => Update::Unknown,
                false => Update::Off,
            };
            Response::Status {
                status: Status {
                    update,
                    ..Status::default()
                },
            }
        }
        None if matches!(request, Request::Questions) && !paths.state_dir.exists() => {
            Response::Questions {
                questions: Vec::new(),
            }
        }
        None if matches!(request, Request::AnsweredQuestion { .. })
            && !paths.state_dir.exists() =>
        {
            Response::AnsweredQuestion { question: None }
        }
        None if matches!(request, Request::Resume) && !paths.state_dir.exists() => {
            Response::NotPaused
        }
        None => {
            let driver = Driver::open(paths.clone()).map_err(|error| match error {
                DriverError::Busy(_) => CliError::from(busy(paths)),
                other => CliError::from(other),
            })?;
            handle(&driver, request).await
        }
    };
    match response {
        Response::Error {
            message,
            code,
            reason,
            next,
        } => Err(CliError {
            reason,
            next,
            ..CliError::new(code, message)
        }),
        response => Ok(response),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use koidrive_driver::{Answer, Question};
    use koidrive_ipc::{Code, Next, bind};

    use super::*;

    #[tokio::test]
    async fn requests_reach_the_daemon_through_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let driver = Driver::open(paths.clone()).unwrap();
        let server = tokio::spawn(serve(bind(&paths.socket()).unwrap(), driver.clone()));
        let mode = fs::metadata(paths.socket()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let response = run(&paths, Request::Status).await.unwrap();
        assert_eq!(
            response,
            Response::Status {
                status: driver.status()
            }
        );
        let error = run(
            &paths,
            Request::Answer {
                question_id: "q1".into(),
                answer: Answer::Delete,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.message,
            "no unanswered question has the id \"q1\"; koi resolve lists the decisions waiting now"
        );
        assert_eq!(error.code, Code::StaleDecision);
        assert_eq!(error.next, [Next::of(&["koi", "resolve", "--json"])]);
        let short = Request::Share {
            path: dir.path().join("report.docx"),
            expiry: koidrive_driver::Expiry::Default,
            password: Some(koidrive_driver::Secret("short".into())),
        };
        let error = run(&paths, short).await.unwrap_err();
        assert_eq!(
            (error.code, error.reason),
            (Code::Usage, Some(koidrive_ipc::Reason::PasswordLength))
        );
        server.abort();
    }

    async fn next_state(lines: &mut koidrive_ipc::Watch) -> Status {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next()).await;
        match line.unwrap().unwrap() {
            Some(Response::State { status, questions }) => {
                assert_eq!(questions.len(), status.folders.len(), "{questions:?}");
                status
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_watcher_gets_the_state_at_once_and_again_after_a_pause_and_an_unlink() {
        let (_dir, paths, root) = linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        let server = tokio::spawn(serve(bind(&paths.socket()).unwrap(), driver.clone()));
        let uid = rustix::process::getuid().as_raw();
        let mut lines = koidrive_ipc::watch(&paths.socket(), uid)
            .await
            .unwrap()
            .unwrap();
        let first = next_state(&mut lines).await;
        assert_eq!((first.paused, first.folders.len()), (false, 1));

        assert_eq!(run(&paths, Request::Pause).await.unwrap(), Response::Paused);
        assert!(next_state(&mut lines).await.paused);
        let unlinked = run(&paths, Request::Unlink { folder: root }).await.unwrap();
        assert!(
            matches!(unlinked, Response::Unlinked { .. }),
            "{unlinked:?}"
        );
        let mut status = next_state(&mut lines).await;
        while !status.folders.is_empty() {
            status = next_state(&mut lines).await;
        }
        assert!(status.paused);
        assert_eq!(
            run(&paths, Request::Status).await.unwrap(),
            Response::Status {
                status: driver.status()
            }
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_watch_ends_when_its_client_hangs_up_and_needs_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Driver::open(Paths::under(dir.path())).unwrap();
        let (client, daemon) = UnixStream::pair().unwrap();
        let answering = tokio::spawn({
            let driver = driver.clone();
            async move { answer(daemon, &driver).await }
        });
        let (reader, mut writer) = client.into_split();
        write_json(&mut writer, &Request::Watch).await.unwrap();
        let line = read_line(reader).await.unwrap();
        assert!(line.starts_with(r#"{"response":"state""#), "{line}");
        drop(writer);
        tokio::time::timeout(Duration::from_secs(30), answering)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Response::Error { code, .. } = handle(&driver, Request::Watch).await else {
            panic!("a watch ran without a daemon");
        };
        assert_eq!(code, Code::Usage);
    }

    #[tokio::test]
    async fn without_a_daemon_a_command_opens_the_profile_itself() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let response = run(&paths, Request::Questions).await.unwrap();
        assert_eq!(response, Response::Questions { questions: vec![] });

        let _held = Driver::open(paths.clone()).unwrap();
        let error = run(&paths, Request::Status).await.unwrap_err();
        assert!(error.message.contains("does not answer"), "{error:?}");
    }

    pub(crate) const SPACE: &str = "0190f0a0-0000-7000-8000-000000000001";

    pub(crate) fn linked_profile() -> (tempfile::TempDir, Paths, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Koidrive");
        fs::create_dir_all(&root).unwrap();
        let store_path = paths.store(SPACE).unwrap();
        fs::create_dir_all(store_path.parent().unwrap()).unwrap();
        fs::set_permissions(&paths.state_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let question = Question {
            id: "q1".into(),
            lineage: "q1".into(),
            space_id: SPACE.into(),
            asked_at_ms: 10,
            recovery_cause: None,
            kind: koidrive_driver::QuestionKind::AmbiguousDisappearance,
            items: Vec::new(),
        };
        koidrive_store::Store::open(&store_path)
            .unwrap()
            .transaction(|tx| {
                tx.set_link(&koidrive_store::Link {
                    space_id: SPACE.into(),
                    space_name: "Projects".into(),
                    root: root.clone(),
                    root_dev: 1,
                    server_url: "https://files.example.com/".into(),
                    device_id: "d1".into(),
                    linked_at_ms: 1,
                })?;
                tx.put_question(&koidrive_store::StoredQuestion {
                    question_id: question.id.clone(),
                    asked_at_ms: question.asked_at_ms,
                    body: serde_json::to_vec(&question).unwrap(),
                    answer: None,
                })
            })
            .unwrap();
        (dir, paths, root)
    }

    #[tokio::test]
    async fn a_leave_without_the_server_changes_nothing_until_confirmed_and_keeps_the_files() {
        let (_dir, paths, root) = linked_profile();
        fs::write(root.join("kept.txt"), b"kept").unwrap();
        koidrive_driver::Credential {
            server: "http://127.0.0.1:9/".into(),
            user_id: "u1".into(),
            user_name: None,
            device_id: "d1".into(),
            device_name: "Mac".into(),
            refresh_credential: "not-a-secret".into(),
        }
        .save_new(&paths.credential())
        .unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        let leave = |confirm_last_device| {
            handle(
                &driver,
                Request::Leave {
                    confirm_last_device,
                },
            )
        };

        let Response::Error { code, next, .. } = leave(false).await else {
            panic!("a sign-out the server did not confirm went on");
        };
        assert_eq!(code, Code::Unavailable);
        assert_eq!(
            next,
            [Next::of(&["koi", "device", "leave", "--last-device"])]
        );
        assert!(paths.credential().exists());
        assert_eq!(driver.status().folders.len(), 1);

        assert_eq!(
            leave(true).await,
            Response::SignedOut {
                not_removed: Vec::new()
            }
        );
        assert!(!paths.credential().exists());
        let status = driver.status();
        assert!(
            status.account.is_none() && status.folders.is_empty(),
            "{status:?}"
        );
        assert_eq!(fs::read(root.join("kept.txt")).unwrap(), b"kept");
    }

    #[tokio::test]
    async fn resolve_reads_a_decision_sync_took_so_the_same_word_is_unchanged_and_another_is_stale()
    {
        use koidrive_engine::question::{ConflictFile, Revisions};

        let (_dir, paths, _root) = linked_profile();
        let file = |node_id: &str| ConflictFile {
            node_id: node_id.into(),
            revisions: Revisions {
                content_rev: 2,
                meta_rev: 1,
            },
        };
        let conflict = Question {
            id: "c1".into(),
            lineage: "c1".into(),
            space_id: SPACE.into(),
            asked_at_ms: 20,
            recovery_cause: None,
            kind: koidrive_driver::QuestionKind::Conflict {
                original: file("n-original"),
                copy: file("n-copy"),
                sent_in: None,
            },
            items: Vec::new(),
        };
        let store = || koidrive_store::Store::open(paths.store(SPACE).unwrap()).unwrap();
        store()
            .transaction(|tx| {
                tx.put_question(&koidrive_store::StoredQuestion {
                    question_id: conflict.id.clone(),
                    asked_at_ms: conflict.asked_at_ms,
                    body: serde_json::to_vec(&conflict).unwrap(),
                    answer: None,
                })
            })
            .unwrap();
        let resolve =
            |id: &str, word: &str| crate::resolve(&paths, Some(id.into()), Some(word.into()));
        let outcome = |done: Result<crate::report::Done, CliError>| match done {
            Ok(done) => Ok(done.outcome),
            Err(error) => Err(error.code),
        };
        use crate::report::Outcome;

        assert_eq!(
            outcome(resolve("c1", "keep-node:n-original").await),
            Ok(Outcome::Accepted)
        );
        assert_eq!(
            outcome(resolve("c1", "keep-node:n-original").await),
            Ok(Outcome::Unchanged)
        );
        assert!(store().transaction(|tx| tx.take_answer("c1")).unwrap());
        assert_eq!(
            outcome(resolve("c1", "keep-node:n-original").await),
            Ok(Outcome::Unchanged)
        );
        for other in ["keep-node:n-copy", "keep-both"] {
            assert_eq!(
                outcome(resolve("c1", other).await),
                Err(Code::StaleDecision),
                "{other}"
            );
        }

        assert_eq!(
            outcome(resolve("q1", "delete").await),
            Ok(Outcome::Accepted)
        );
        store().transaction(|tx| tx.remove_question("q1")).unwrap();
        assert_eq!(
            outcome(resolve("q1", "delete").await),
            Err(Code::StaleDecision)
        );
    }

    #[tokio::test]
    async fn a_repeated_answer_is_unchanged_after_sync_took_it_and_another_answer_is_stale() {
        let (_dir, paths, _root) = linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        let answer = |question_id: &str, answer| {
            handle(
                &driver,
                Request::Answer {
                    question_id: question_id.into(),
                    answer,
                },
            )
        };
        assert_eq!(answer("q1", Answer::Delete).await, Response::Answered);
        assert_eq!(
            answer("q1", Answer::Delete).await,
            Response::AlreadyAnswered
        );
        assert!(
            koidrive_store::Store::open(paths.store(SPACE).unwrap())
                .unwrap()
                .transaction(|tx| tx.take_answer("q1"))
                .unwrap()
        );
        assert!(driver.questions().unwrap().is_empty());
        assert_eq!(
            answer("q1", Answer::Delete).await,
            Response::AlreadyAnswered
        );

        let Response::Error {
            code,
            message,
            next,
            ..
        } = answer("q1", Answer::Restore).await
        else {
            panic!("another answer was taken");
        };
        assert_eq!(code, Code::StaleDecision);
        assert_eq!(
            message,
            "the question was answered with delete; koi resolve lists the decisions waiting now"
        );
        assert_eq!(next, [Next::of(&["koi", "resolve", "--json"])]);
    }

    #[tokio::test]
    async fn an_answer_the_daemon_acknowledged_outlives_the_daemon_and_is_unchanged_after_sync_took_it()
     {
        let (_dir, paths, _root) = linked_profile();
        let answer = || Request::Answer {
            question_id: "q1".into(),
            answer: Answer::Delete,
        };
        let driver = Driver::open(paths.clone()).unwrap();
        assert_eq!(handle(&driver, answer()).await, Response::Answered);
        drop(driver);
        let mut store = koidrive_store::Store::open(paths.store(SPACE).unwrap()).unwrap();
        assert!(store.transaction(|tx| tx.take_answer("q1")).unwrap());
        drop(store);
        let driver = Driver::open(paths.clone()).unwrap();
        assert_eq!(handle(&driver, answer()).await, Response::AlreadyAnswered);
    }

    #[tokio::test]
    async fn question_ids_that_differ_in_case_are_different_questions() {
        let (_dir, paths, _root) = linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        let answer = |question_id: &str| {
            handle(
                &driver,
                Request::Answer {
                    question_id: question_id.into(),
                    answer: Answer::Delete,
                },
            )
        };
        assert_eq!(answer("q1").await, Response::Answered);
        koidrive_store::Store::open(paths.store(SPACE).unwrap())
            .unwrap()
            .transaction(|tx| tx.take_answer("q1"))
            .unwrap();
        let Response::Error { code, .. } = answer("Q1").await else {
            panic!("the answer to q1 was taken as one to Q1");
        };
        assert_eq!(code, Code::StaleDecision);
        assert_eq!(answer("q1").await, Response::AlreadyAnswered);
    }

    #[tokio::test]
    async fn an_answer_to_a_decision_replaced_before_sync_took_it_is_stale() {
        let (_dir, paths, _root) = linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        let answer = || {
            handle(
                &driver,
                Request::Answer {
                    question_id: "q1".into(),
                    answer: Answer::Delete,
                },
            )
        };
        assert_eq!(answer().await, Response::Answered);
        let replacement = Question {
            id: "q2".into(),
            lineage: "q2".into(),
            space_id: SPACE.into(),
            asked_at_ms: 20,
            recovery_cause: None,
            kind: koidrive_driver::QuestionKind::AmbiguousDisappearance,
            items: Vec::new(),
        };
        koidrive_store::Store::open(paths.store(SPACE).unwrap())
            .unwrap()
            .transaction(|tx| {
                tx.remove_question("q1")?;
                tx.put_question(&koidrive_store::StoredQuestion {
                    question_id: "q2".into(),
                    asked_at_ms: 20,
                    body: serde_json::to_vec(&replacement).unwrap(),
                    answer: None,
                })
            })
            .unwrap();
        assert_eq!(driver.questions().unwrap(), [replacement]);
        let Response::Error { code, next, .. } = answer().await else {
            panic!("the dropped answer was reported as taken");
        };
        assert_eq!(code, Code::StaleDecision);
        assert_eq!(next, [Next::of(&["koi", "resolve", "--json"])]);
    }

    #[tokio::test]
    async fn share_link_requests_cannot_be_recorded_even_inside_nested_once_requests() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let driver = Driver::open(paths.clone()).unwrap();
        for request in [
            Request::ShareLinks,
            Request::Share {
                path: dir.path().join("file.txt"),
                expiry: koidrive_driver::Expiry::Default,
                password: None,
            },
            Request::RevokeShareLink {
                link: koidrive_driver::Secret("test-link".into()),
            },
        ] {
            for depth in 1..=3 {
                let mut wrapped = request.clone();
                for level in 0..depth {
                    wrapped = wrapped.once(
                        Some(RequestId::parse(&format!("share-{depth}-{level}")).unwrap()),
                        Key::restart(dir.path()),
                    );
                }
                assert!(matches!(
                    handle(&driver, wrapped).await,
                    Response::Error {
                        code: Code::Usage,
                        ..
                    }
                ));
                assert!(!paths.requests().exists());
            }
        }
        assert!(matches!(
            handle(&driver, Request::ShareLinks).await,
            Response::Error {
                code: Code::AuthRequired,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_restart_that_leaves_the_folder_halted_records_nothing_so_its_id_runs_again() {
        let (_dir, paths, root) = linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        let request_id = RequestId::parse("r1").unwrap();
        let restart = || {
            let key = Key::restart(&root);
            Request::Restart {
                folder: root.clone(),
            }
            .once(Some(request_id.clone()), key)
        };
        for _ in 0..2 {
            let Response::Restarted { folder } = handle(&driver, restart()).await else {
                panic!("the restart failed");
            };
            assert_eq!(folder.state, FolderState::Halted, "{folder:?}");
            assert!(!crate::request::request_file(&paths, &request_id).exists());
        }
        driver.stop().await;
    }

    #[tokio::test]
    async fn a_request_with_an_id_runs_once_and_its_record_outlives_the_folder_and_the_daemon() {
        let (_dir, paths, root) = linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        let id = |text: &str| Some(RequestId::parse(text).unwrap());
        let restart = |folder: &Path, request_id| {
            let key = Key::restart(folder);
            Request::Restart {
                folder: folder.to_path_buf(),
            }
            .once(request_id, key)
        };
        let first = handle(&driver, restart(&root, id("r1"))).await;
        assert!(matches!(first, Response::Restarted { .. }), "{first:?}");
        let unlinked = handle(
            &driver,
            Request::Unlink {
                folder: root.clone(),
            },
        )
        .await;
        assert!(
            matches!(unlinked, Response::Unlinked { .. }),
            "{unlinked:?}"
        );
        assert_eq!(
            handle(
                &driver,
                Request::Unlink {
                    folder: root.clone()
                }
            )
            .await,
            Response::NotLinked { path: root.clone() }
        );
        assert_eq!(handle(&driver, restart(&root, id("r1"))).await, first);
        drop(driver);
        let driver = Driver::open(paths.clone()).unwrap();
        assert_eq!(handle(&driver, restart(&root, id("r1"))).await, first);

        let other = handle(&driver, restart(&root.join("Other"), id("r1"))).await;
        assert!(
            matches!(
                &other,
                Response::Error {
                    code: Code::StaleDecision,
                    ..
                }
            ),
            "{other:?}"
        );
        for _ in 0..2 {
            let failed = handle(&driver, restart(&root, id("r2"))).await;
            assert!(
                matches!(
                    &failed,
                    Response::Error {
                        code: Code::Usage,
                        ..
                    }
                ),
                "{failed:?}"
            );
            assert!(!crate::request::request_file(&paths, &id("r2").unwrap()).exists());
        }
    }
}
