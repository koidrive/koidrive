// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use koidrive_driver::{
    Answered, Change, Driver, DriverError, FolderState, FolderStatus, Linked, Paths, Question,
    Status, Update,
};
use koidrive_engine::commit::Lift;
use koidrive_ipc::{
    Blocked, Code, Key, Request, RequestId, Response, read_line, write_json, write_line,
};
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
            human: error.human,
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
    let fixes = matches!(inner, Request::Relink { .. } | Request::Lift { .. });
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
        Response::Restarted { folder } => !fixes && folder.state == FolderState::Halted,
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
    let read_only = matches!(
        &request,
        Request::Status
            | Request::Access
            | Request::Questions
            | Request::ShareLinks
            | Request::Gitignore
            | Request::GitignoreOnly { .. }
    );
    let outcome: Result<Response, DriverError> = match request {
        Request::Status => Ok(Response::Status {
            status: driver.status(),
        }),
        Request::Questions => driver
            .questions()
            .map(|questions| Response::Questions { questions }),
        Request::AnsweredQuestion { question_id } => driver
            .answered_question(&question_id)
            .map(|question| Response::AnsweredQuestion { question }),
        Request::Restores { space_id } => driver
            .restores(&space_id)
            .map(|restores| Response::Restores { restores }),
        Request::Pair { server, code, name } => match driver.pair(&server, &code, &name).await {
            Ok((account, set_aside)) => Ok(Response::Paired { account, set_aside }),
            Err(error) => return Response::from(CliError::code_refused(error)),
        },
        Request::Claim {
            server,
            code,
            user_name,
            name,
        } => match driver.claim(&server, &code, &user_name, &name).await {
            Ok((account, set_aside)) => Ok(Response::Paired { account, set_aside }),
            Err(error) => return Response::from(CliError::code_refused(error)),
        },
        Request::SaveCredential { credential } => driver
            .save_credential(credential)
            .await
            .map(|(account, set_aside)| Response::Paired { account, set_aside }),
        Request::SignOut { device_id } => {
            driver
                .sign_out_device(&device_id)
                .await
                .map(|left| Response::SignedOut {
                    not_removed: left.not_removed,
                    unconfirmed: left.unconfirmed,
                    set_aside: left.set_aside,
                })
        }
        Request::Leave {
            confirm_last_device,
        } => driver
            .sign_out(confirm_last_device)
            .await
            .map(|left| Response::SignedOut {
                not_removed: left.not_removed,
                unconfirmed: left.unconfirmed,
                set_aside: left.set_aside,
            }),
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
            Ok((link, set_aside)) => Ok(Response::Unlinked {
                space_id: link.space_id,
                path: link.root,
                set_aside,
            }),
            Err(DriverError::NotLinked(path)) => Ok(Response::NotLinked { path }),
            Err(error) => Err(error),
        },
        Request::UnlinkSpace { space_id } => {
            (driver.unlink_space(&space_id).await).map(|link| match link {
                Some((link, set_aside)) => Response::Unlinked {
                    space_id: link.space_id,
                    path: link.root,
                    set_aside,
                },
                None => Response::UnlinkedSpace { space_id },
            })
        }
        Request::Restart { folder } => driver
            .restart(&folder)
            .await
            .map(|folder| Response::Restarted { folder }),
        Request::Lift { folder, replan } => {
            let lift = if replan { Lift::Replan } else { Lift::Resend };
            (driver.lift(&folder, lift).await).map(|folder| Response::Restarted { folder })
        }
        Request::Relink { folder } => driver
            .relink(&folder)
            .await
            .map(|folder| Response::Restarted { folder }),
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
        Request::CheckForUpdates => {
            return match driver.check_for_updates_now().await {
                Ok(update) => Response::UpdateChecked {
                    update,
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                },
                Err(error) => Response::from(CliError::from(error)),
            };
        }
        Request::Gitignore => Ok(Response::Gitignore {
            follows: driver.follows_gitignore(),
        }),
        Request::SetGitignore { follow } => {
            let already = driver.follows_gitignore() == follow;
            (driver.set_gitignore(follow).await).map(|()| Response::GitignoreSet {
                follows: follow,
                already,
            })
        }
        Request::GitignoreOnly { folder } => {
            let driver = driver.clone();
            match tokio::task::spawn_blocking(move || driver.gitignore_only(&folder)).await {
                Ok(counted) => counted.map(|only| Response::GitignoreOnly {
                    files: only.files,
                    bytes: only.bytes,
                }),
                Err(error) => {
                    return Response::from(CliError::from(format!(
                        "counting the files failed: {error}"
                    )));
                }
            }
        }
        Request::Once {
            request_id,
            key,
            run,
        } => return once(driver, request_id, key, *run).await,
    };
    outcome.unwrap_or_else(|error| {
        Response::from(if read_only {
            CliError::retry_safe(error)
        } else {
            CliError::from(error)
        })
    })
}

static READING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// The linked folders this process may not read. macOS keeps a launch agent out of the folders that its privacy
/// settings guard, such as Documents, until the person allows it, and asks while the first read waits. Each read
/// runs on a thread of its own, which a read waiting for the person keeps until the answer, so the runtime never waits
/// for it. A folder whose earlier read still waits gets no second thread, however often the status is asked for.
async fn unreadable(folders: Vec<FolderStatus>) -> Vec<Blocked> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let reads: Vec<_> = (folders.into_iter())
        .filter_map(|folder| folder.path)
        .map(|path| {
            let (send, receive) = tokio::sync::oneshot::channel();
            if READING.lock().unwrap().insert(path.clone()) {
                let reading = path.clone();
                std::thread::spawn(move || {
                    let read = fs::read_dir(&reading).map(drop);
                    READING.lock().unwrap().remove(&reading);
                    send.send(read)
                });
            }
            (path, receive)
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

pub const STARTING: Duration = Duration::from_secs(15);

pub enum Reached<T> {
    Daemon(Box<Response>),
    Profile(T),
}

pub const REPLY: Duration = Duration::from_secs(5);

pub async fn ask(paths: &Paths, request: &Request) -> Result<Option<Response>, CliError> {
    let socket = paths.socket();
    let asked = async {
        match koidrive_ipc::send(&socket, request, rustix::process::getuid().as_raw()).await {
            Err(error)
                if error.kind() == io::ErrorKind::UnexpectedEof
                    && matches!(request, Request::Status) =>
            {
                Ok(None)
            }
            answer => answer.map_err(|error| socket_error(&socket, error)),
        }
    };
    if !matches!(request, Request::Status) {
        return asked.await;
    }
    match tokio::time::timeout(REPLY, asked).await {
        Ok(answer) => answer,
        Err(_) => Err(CliError::from(format!(
            "the daemon did not answer within {} seconds",
            REPLY.as_secs()
        )).human(format!("Background sync did not answer within {} seconds. If it stays unresponsive, run koi diagnose to collect details.", REPLY.as_secs()))),
    }
}

pub async fn reach<T>(
    paths: &Paths,
    request: &Request,
    within: Duration,
    open: impl Fn() -> Result<T, DriverError>,
) -> Result<Reached<T>, CliError> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match open() {
            Ok(opened) => return Ok(Reached::Profile(opened)),
            Err(DriverError::Busy(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(DriverError::Busy(_)) => {
                return Err(CliError::new(
                    Code::Unavailable,
                    format!(
                        "a Koidrive process holds this profile, but its socket {} did not answer within {} seconds",
                        paths.socket().display(),
                        within.as_secs()
                    ),
                ).human("Another Koidrive process holds this profile, but background sync did not answer. Run koi diagnose before changing local sync data or starting another process."));
            }
            Err(other) => return Err(CliError::from(other)),
        }
        if let Some(response) = ask(paths, request).await? {
            return Ok(Reached::Daemon(Box::new(response)));
        }
    }
}

pub(crate) fn socket_error(socket: &Path, error: io::Error) -> CliError {
    let human = match error.kind() {
        io::ErrorKind::PermissionDenied => {
            "Koidrive could not access background sync. Check local file permissions and run koi status."
        }
        io::ErrorKind::InvalidData => {
            "Background sync returned a response Koidrive could not read. Run koi diagnose to collect a report."
        }
        _ => "The connection to background sync failed. Run koi status to check it.",
    };
    CliError::from(format!("the daemon's socket {} failed: {error}", socket.display()))
        .human(format!("{human} The command's result is unknown. Check the affected item before repeating a command that changes it."))
}

pub async fn run(paths: &Paths, request: Request) -> Result<Response, CliError> {
    let response = match ask(paths, &request).await? {
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
        None if matches!(request, Request::Gitignore) && !paths.state_dir.exists() => {
            match koidrive_driver::Config::load(&paths.config_file) {
                Ok(config) => Response::Gitignore {
                    follows: config.sync.gitignore,
                },
                Err(error) => Response::from(CliError::retry_safe(DriverError::from(error))),
            }
        }
        None => match reach(paths, &request, STARTING, || {
            Driver::open_with_telemetry(paths.clone(), crate::telemetry::recorder())
        })
        .await?
        {
            Reached::Daemon(response) => *response,
            Reached::Profile(driver) if matches!(request, Request::Questions) => {
                match driver.into_questions().await {
                    Ok(questions) => Response::Questions { questions },
                    Err(error) => Response::from(CliError::retry_safe(error)),
                }
            }
            Reached::Profile(driver) => handle(&driver, request).await,
        },
    };
    answered(response)
}

pub(crate) fn answered(response: Response) -> Result<Response, CliError> {
    match response {
        Response::Error {
            message,
            human,
            code,
            reason,
            next,
        } => Err(CliError {
            reason,
            human,
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

    #[tokio::test]
    async fn the_gitignore_setting_is_read_set_and_counted_through_the_daemon() {
        let (_dir, paths, root) = linked_profile();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/a.js"), "abc").unwrap();
        fs::write(root.join("notes.txt"), "synced").unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        let server = tokio::spawn(serve(bind(&paths.socket()).unwrap(), driver.clone()));
        let ask = |request| run(&paths, request);
        let counted = Response::GitignoreOnly { files: 1, bytes: 3 };

        assert_eq!(
            ask(Request::Gitignore).await.unwrap(),
            Response::Gitignore { follows: true }
        );
        let count = || Request::GitignoreOnly {
            folder: root.clone(),
        };
        assert_eq!(ask(count()).await.unwrap(), counted);
        for already in [false, true] {
            assert_eq!(
                ask(Request::SetGitignore { follow: false }).await.unwrap(),
                Response::GitignoreSet {
                    follows: false,
                    already
                }
            );
        }
        assert_eq!(
            ask(Request::Gitignore).await.unwrap(),
            Response::Gitignore { follows: false }
        );
        assert_eq!(ask(count()).await.unwrap(), counted);
        let not_linked = Request::GitignoreOnly {
            folder: root.join("node_modules"),
        };
        let error = ask(not_linked).await.unwrap_err();
        assert!(
            error.message.ends_with("is not a linked folder"),
            "{}",
            error.message
        );
        server.abort();
    }

    #[tokio::test]
    async fn without_sync_state_the_gitignore_setting_comes_from_the_configuration_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        assert_eq!(
            run(&paths, Request::Gitignore).await.unwrap(),
            Response::Gitignore { follows: true }
        );
        fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        fs::write(&paths.config_file, "[sync]\ngitignore = false\n").unwrap();
        assert_eq!(
            run(&paths, Request::Gitignore).await.unwrap(),
            Response::Gitignore { follows: false }
        );
        assert!(!paths.state_dir.exists());
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
    }

    #[tokio::test]
    async fn an_update_check_reaches_the_daemon_or_else_the_profile() {
        assert!(koidrive_driver::update::is_development_version(env!(
            "CARGO_PKG_VERSION"
        )));
        let checked = Response::UpdateChecked {
            update: Update::Off,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        assert_eq!(
            run(&paths, Request::CheckForUpdates).await.unwrap(),
            checked
        );
        let driver = Driver::open(paths.clone()).unwrap();
        let socket = bind(&paths.socket()).unwrap();
        let daemon = tokio::spawn(async move { serve(socket, driver).await });
        assert_eq!(
            ask(&paths, &Request::CheckForUpdates).await.unwrap(),
            Some(checked)
        );
        daemon.abort();
    }

    #[tokio::test]
    async fn a_command_waits_for_a_daemon_that_holds_the_profile_before_its_socket_answers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let driver = Driver::open(paths.clone()).unwrap();
        let socket = paths.socket();
        let daemon = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            serve(bind(&socket).unwrap(), driver).await;
        });
        let response = run(&paths, Request::Status).await.unwrap();
        assert!(matches!(response, Response::Status { .. }), "{response:?}");
        daemon.abort();
    }

    #[tokio::test]
    async fn a_status_request_cut_off_by_an_exiting_daemon_reaches_the_profile_instead() {
        let (_dir, paths, _root) = linked_profile();
        let listener = bind(&paths.socket()).unwrap();
        let daemon = tokio::spawn(async move {
            loop {
                drop(listener.accept().await.unwrap());
            }
        });
        let response = run(&paths, Request::Status).await.unwrap();
        assert!(
            matches!(&response, Response::Status { status } if !status.running),
            "{response:?}"
        );
        daemon.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_status_request_to_a_daemon_that_never_replies_fails_after_the_reply_limit() {
        let (_dir, paths, _root) = linked_profile();
        let listener = bind(&paths.socket()).unwrap();
        let daemon = tokio::spawn(async move {
            let _held = listener.accept().await.unwrap();
            std::future::pending::<()>().await
        });
        let started = tokio::time::Instant::now();
        let wall = std::time::Instant::now();
        let error = run(&paths, Request::Status).await.unwrap_err();
        assert!(started.elapsed() < REPLY + Duration::from_secs(2));
        assert!(wall.elapsed() < REPLY);
        assert_eq!(error.message, "the daemon did not answer within 5 seconds");
        daemon.abort();
    }

    #[tokio::test]
    async fn a_profile_held_by_a_daemon_that_never_answers_is_a_retryable_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let _held = Driver::open(paths.clone()).unwrap();
        let started = tokio::time::Instant::now();
        let Err(error) = reach(&paths, &Request::Status, Duration::from_secs(1), || {
            Driver::open(paths.clone())
        })
        .await
        else {
            panic!("a held profile was opened");
        };
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert_eq!(error.code, Code::Unavailable);
        assert!(error.code.retryable());
        assert_eq!(
            error.message,
            format!(
                "a Koidrive process holds this profile, but its socket {} did not answer within 1 seconds",
                paths.socket().display()
            )
        );
    }

    async fn conflict_with_a_hanging_device_list() -> (
        tempfile::TempDir,
        Paths,
        tokio::sync::mpsc::UnboundedReceiver<()>,
        tokio::task::JoinHandle<()>,
    ) {
        use koidrive_driver::{HeldItem, QuestionKind};
        use koidrive_engine::question::{ConflictFile, Revisions};
        use koidrive_proto::v1::{AccessToken, NodeState, VersionInfo};
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        let (listed, listing) = tokio::sync::mpsc::unbounded_channel();
        let served = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let read = socket.read(&mut request).await.unwrap();
                let line = String::from_utf8_lossy(&request[..read])
                    .lines()
                    .next()
                    .map(str::to_owned);
                if line.is_some_and(|line| line.contains(" /v1/devices")) {
                    held.push(socket);
                    listed.send(()).unwrap();
                    continue;
                }
                let body = prost::Message::encode_to_vec(&AccessToken {
                    token: "token".into(),
                    expires_at_ms: i64::MAX,
                    ..Default::default()
                });
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        let (dir, paths, _root) = linked_profile();
        koidrive_driver::Credential {
            server,
            user_id: "u1".into(),
            user_name: None,
            device_id: "d1".into(),
            device_name: "Mac".into(),
            refresh_credential: "not-a-secret".into(),
        }
        .save_new(&paths.credential())
        .unwrap();
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
            files: 1,
            device_id: None,
            device_name: None,
            size: None,
            modified_at_ms: None,
            restore: None,
        };
        let conflict = Question {
            id: "c1".into(),
            lineage: "c1".into(),
            space_id: SPACE.into(),
            asked_at_ms: 20,
            recovery_cause: None,
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
        koidrive_store::Store::open(paths.store(SPACE).unwrap())
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
                tx.put_question(&koidrive_store::StoredQuestion {
                    question_id: "c1".into(),
                    asked_at_ms: 20,
                    body: serde_json::to_vec(&conflict).unwrap(),
                    answer: None,
                })
            })
            .unwrap();
        (dir, paths, listing, served)
    }

    #[tokio::test]
    async fn requests_for_this_devices_state_answer_while_the_server_hangs() {
        let (_dir, paths, mut listing, served) = conflict_with_a_hanging_device_list().await;
        let driver = Driver::open(paths).unwrap();
        for request in [
            Request::Questions,
            Request::Status,
            Request::AnsweredQuestion {
                question_id: "c1".into(),
            },
            Request::Restores {
                space_id: SPACE.into(),
            },
        ] {
            let response = tokio::time::timeout(Duration::from_secs(1), handle(&driver, request))
                .await
                .expect("a request for this device's state waited for the server");
            assert!(!matches!(response, Response::Error { .. }), "{response:?}");
        }
        listing.recv().await.unwrap();
        served.abort();
    }

    #[tokio::test]
    async fn questions_read_without_a_daemon_free_the_profile_while_the_device_list_hangs() {
        let (_dir, paths, mut listing, served) = conflict_with_a_hanging_device_list().await;
        let reading = tokio::spawn({
            let paths = paths.clone();
            async move { run(&paths, Request::Questions).await }
        });
        listing.recv().await.unwrap();
        drop(Driver::open(paths).expect("the profile stayed held while the server was asked"));
        let Response::Questions { questions } = reading.await.unwrap().unwrap() else {
            panic!("the questions were not read");
        };
        assert_eq!(questions.len(), 2);
        served.abort();
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
                    root_volume: None,
                    server_url: "https://files.example.com/".into(),
                    device_id: "d1".into(),
                    linked_at_ms: 1,
                    found_marker: false,
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
            server: format!("http://{}/", koidrive_test_support::reset_tcp_address()),
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

        let Response::SignedOut {
            not_removed,
            unconfirmed: Some(_),
            set_aside,
        } = leave(true).await
        else {
            panic!("a leave without the server must say the revocation is unconfirmed");
        };
        assert_eq!(not_removed, []);
        assert_eq!(set_aside, koidrive_driver::SetAside::default());
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
        let resolve = |id: &str, word: &str| {
            crate::resolve(&paths, Some(id.into()), Some(word.into()), false)
        };
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
    async fn a_relink_that_leaves_the_folder_halted_is_recorded_so_its_id_replays_it() {
        let (_dir, paths, root) = linked_profile();
        let dir = fs::File::open(&root).unwrap();
        let found = koidrive_fs::root_at(std::os::fd::AsFd::as_fd(&dir)).unwrap();
        let token = |digit: &str| Some(digit.repeat(32));
        let marker = koidrive_fs::Marker {
            space_id: SPACE.into(),
            effects: 0,
            token: token("b"),
        };
        koidrive_fs::write_marker(&root, &marker).unwrap();
        let mut store = koidrive_store::Store::open(paths.store(SPACE).unwrap()).unwrap();
        store
            .transaction(|tx| {
                let link = tx.link()?.unwrap();
                tx.set_link(&koidrive_store::Link {
                    root_dev: found.dev,
                    root_volume: found.volume,
                    ..link
                })?;
                tx.set_root_tokens(&koidrive_fs::RootTokens {
                    current: token("a"),
                    pending: None,
                })
            })
            .unwrap();
        drop(store);
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        let request_id = RequestId::parse("r1").unwrap();
        let relink = || {
            let folder = root.clone();
            Request::Relink { folder }.once(Some(request_id.clone()), Key::relink(&root))
        };
        for _ in 0..2 {
            let Response::Restarted { folder } = handle(&driver, relink()).await else {
                panic!("the relink failed");
            };
            assert_eq!(folder.state, FolderState::Halted, "{folder:?}");
            assert!(crate::request::request_file(&paths, &request_id).exists());
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
