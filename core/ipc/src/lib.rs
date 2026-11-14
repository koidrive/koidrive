// SPDX-License-Identifier: Apache-2.0

//! The protocol of the daemon's local socket and its client. A client sends one [`Request`] as a JSON line and reads
//! one [`Response`] line back, one exchange per connection. [`Request::Watch`] is the exception: the daemon keeps that
//! connection open and sends a line on every change, which [`watch`] reads. The socket is the owner's only: [`bind`]
//! makes its directory 0700 and the socket 0600, and [`send`] refuses a socket that another user serves.

mod bind;

use std::io;
use std::path::{Path, PathBuf};

use koidrive_driver::{
    Account, Answer, CleanupFailure, Credential, Expiry, FolderStatus, LinkApproval, Paths,
    Question, RestoreRequest, RestoreStatus, Secret, SharedLink, Status,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

pub use bind::bind;

/// The longest request or response line the socket accepts.
const MAX_LINE: u64 = 16 << 20;

/// The most bytes a request ID has.
const MAX_ID: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    Status,
    Pair {
        server: String,
        code: String,
        name: String,
    },
    /// Claims a new server with the one-time code from its log.
    Claim {
        server: String,
        code: String,
        user_name: String,
        name: String,
    },
    /// Stores the credential that `koi setup` claimed.
    SaveCredential {
        credential: Credential,
    },
    /// Removes the credential of `device_id`, which the server no longer accepts, unlinking every folder; their files
    /// stay. Revoking the device on the server is best effort, since the server already refuses the credential. A
    /// credential saved for another device meanwhile is kept.
    SignOut {
        device_id: String,
    },
    /// `Driver::sign_out`: revokes this device on the server, unlinks every folder and removes the credential. Without
    /// `confirm_last_device`, the account's last active device and a server that does not answer change nothing.
    Leave {
        confirm_last_device: bool,
    },
    Link {
        folder: PathBuf,
        space: String,
    },
    LinkApproved {
        folder: PathBuf,
        space: String,
        approval: LinkApproval,
    },
    Unlink {
        folder: PathBuf,
    },
    Restart {
        folder: PathBuf,
    },
    /// `Driver::lift`: `Lift::Replan` when `replan`, else `Lift::Resend`.
    Lift {
        folder: PathBuf,
        replan: bool,
    },
    Questions,
    /// `Driver::answered_question`.
    AnsweredQuestion {
        question_id: String,
    },
    Answer {
        question_id: String,
        answer: Answer,
    },
    /// Records a restore in the folder of `space_id`, for its engine to send. `space_id`, `folder` and `path`, the folder's
    /// root and the path in it, come back in the response, so that the record of a request ID can report the restore again
    /// when the folder is no longer linked.
    Restore {
        space_id: String,
        restore: RestoreRequest,
        folder: String,
        path: String,
    },
    Restores {
        space_id: String,
    },
    /// The linked folders that this process may not read.
    Access,
    /// `Driver::pause`.
    Pause,
    /// `Driver::resume`.
    Resume,
    /// Keeps the connection open and answers with a [`Response::State`] at once and after each change, until the client
    /// closes the connection.
    Watch,
    /// `Driver::share`: a new link to the file or folder at the absolute `path`.
    Share {
        path: PathBuf,
        expiry: Expiry,
        password: Option<Secret>,
    },
    /// `Driver::share_links`.
    ShareLinks,
    /// `Driver::revoke_share_link`: `link` is a link's ID or URL.
    RevokeShareLink {
        link: Secret,
    },
    /// Runs `run` once for `request_id`: a later request with the same ID and key gets the first response, and one
    /// with another key fails as stale.
    Once {
        request_id: RequestId,
        key: Key,
        run: Box<Request>,
    },
}

impl Request {
    /// This request run once for `request_id`, or as it is without one.
    pub fn once(self, request_id: Option<RequestId>, key: Key) -> Request {
        match request_id {
            Some(request_id) => Request::Once {
                request_id,
                key,
                run: Box::new(self),
            },
            None => self,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Status {
        status: Status,
    },
    Paired {
        account: Account,
    },
    Linked {
        folder: FolderStatus,
    },
    /// The folder was already linked to the space.
    AlreadyLinked {
        folder: FolderStatus,
    },
    Unlinked {
        space_id: String,
        path: PathBuf,
    },
    /// The folder to unlink was not linked.
    NotLinked {
        path: PathBuf,
    },
    Restarted {
        folder: FolderStatus,
    },
    Questions {
        questions: Vec<Question>,
    },
    AnsweredQuestion {
        question: Option<Question>,
    },
    Answered,
    /// The decision was answered before with the same answer.
    AlreadyAnswered,
    Restore {
        id: String,
        space_id: String,
        folder: String,
        path: String,
    },
    Restores {
        restores: Vec<RestoreStatus>,
    },
    SignedOut {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        not_removed: Vec<CleanupFailure>,
    },
    Access {
        blocked: Vec<Blocked>,
    },
    Paused,
    AlreadyPaused,
    Resumed,
    /// Sync was not paused.
    NotPaused,
    /// What a watcher shows: the status and the questions waiting for an answer.
    State {
        status: Status,
        questions: Vec<Question>,
    },
    Shared {
        link: SharedLink,
    },
    ShareLinks {
        links: Vec<SharedLink>,
    },
    ShareLinkRevoked {
        link: SharedLink,
        already: bool,
    },
    /// A failed request. An older daemon sends only `message`, which reads as [`Code::Failed`].
    Error {
        message: String,
        #[serde(default)]
        code: Code,
        #[serde(
            default,
            deserialize_with = "Reason::known",
            skip_serializing_if = "Option::is_none"
        )]
        reason: Option<Reason>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        next: Vec<Next>,
    },
}

/// What kind of failure an error is. Each code has one exit status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// No more specific code applies.
    #[default]
    Failed,
    /// The arguments are invalid, or input the command needs is missing.
    Usage,
    /// `status --check` found a decision that needs a choice.
    DecisionRequired,
    /// Credentials are missing or refused, or a person must log in.
    AuthRequired,
    /// Sync is halted or stopped, and the command cannot go on until that is fixed.
    Halted,
    /// The server or the network is offline, unavailable or limits the rate at the moment.
    Unavailable,
    /// The decision answered no longer exists, or a local request ID was replayed with different arguments or after
    /// its saved invitation expired.
    StaleDecision,
    /// The server rejected a claim or pairing code as wrong or expired.
    WrongCode,
    /// A signal stopped the command, or an earlier run with the same request ID has no recorded outcome.
    Interrupted,
}

impl Code {
    pub fn exit(self) -> u8 {
        match self {
            Code::Failed => 1,
            Code::Usage => 2,
            Code::DecisionRequired => 3,
            Code::AuthRequired => 4,
            Code::Halted => 5,
            Code::Unavailable => 6,
            Code::StaleDecision => 7,
            Code::WrongCode => 8,
            Code::Interrupted => 130,
        }
    }

    /// Whether the same command may succeed later without a change.
    pub fn retryable(self) -> bool {
        self == Code::Unavailable
    }
}

/// What a refusal is about, beside its [`Code`], for a surface that says it in its own words. A refusal without one
/// is only as specific as its code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The path is not in a linked folder.
    NotInLinkedFolder,
    /// The path is a linked folder, and the command needs a file or folder in one.
    LinkedFolderItself,
    /// The server has nothing at the path: no live file, or no deletion in the trash.
    NotFound,
    /// The path is live, so there is nothing to bring back from the trash.
    NotDeleted,
    /// The path is a folder, and the command needs a file.
    IsFolder,
    /// The file has no such version.
    NoSuchVersion,
    /// The version is the file's current one already.
    AlreadyCurrent,
    /// Several deletions had the path, and none was picked.
    SeveralDeletions,
    /// None of the deletions at the path is the node asked for.
    NotInTrash,
    /// The path is on disk but not on the server: it has not synced yet, or the folder's rules leave it out.
    NotOnServer,
    /// A share link's password does not have 8 to 256 characters.
    PasswordLength,
    /// No share link of the account matches the ID or URL.
    NoSuchLink,
    /// The space has as many active share links as the server allows.
    ShareLinkLimit,
}

impl Reason {
    /// Reads an optional reason, and a reason this version does not know as none, so a newer peer's refusal still
    /// reads by its code.
    pub fn known<'de, D: serde::Deserializer<'de>>(reason: D) -> Result<Option<Reason>, D::Error> {
        Ok(Option::<Value>::deserialize(reason)?
            .and_then(|reason| Reason::deserialize(reason).ok()))
    }
}

/// A command to run next, as the arguments to pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Next {
    pub argv: Vec<String>,
}

impl Next {
    pub fn of(argv: &[&str]) -> Self {
        Next {
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }
}

/// A folder that the daemon may not read, as its `access` request finds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Blocked {
    pub folder: String,
    /// The read still waits after two seconds, usually because macOS is asking the person. Otherwise macOS refused it.
    pub waiting: bool,
}

/// An ID an agent picks for one change: 1 to 128 ASCII letters, digits, `-` and `_`. IDs that differ in case are
/// different IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestId(String);

impl RequestId {
    /// The value parser of `--request-id`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let shaped = (1..=MAX_ID).contains(&text.len())
            && (text.chars()).all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        match shaped {
            true => Ok(RequestId(text.to_owned())),
            false => Err(format!(
                "a request ID has 1 to {MAX_ID} ASCII letters, digits, - and _"
            )),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RequestId {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        RequestId::parse(&text)
    }
}

impl From<RequestId> for String {
    fn from(id: RequestId) -> Self {
        id.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a request ID was used for: the command path, such as `folder restart`, and its arguments, normalized. The same
/// ID with another key is a stale request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub command: String,
    pub arguments: Value,
}

impl Key {
    pub fn new(command: &str, arguments: Value) -> Self {
        Key {
            command: command.to_owned(),
            arguments,
        }
    }

    pub fn restart(folder: &Path) -> Self {
        Key::new(
            "folder restart",
            json!({ "folder": folder.as_os_str().as_encoded_bytes() }),
        )
    }

    pub fn retry(folder: &Path, replan: bool) -> Self {
        Key::new(
            "folder retry",
            json!({
                "folder": folder.as_os_str().as_encoded_bytes(),
                "how": if replan { "replan" } else { "resend" },
            }),
        )
    }

    pub fn restore(path: &Path, version: Option<&str>, node: Option<&str>) -> Self {
        Key::new(
            "history restore",
            json!({
                "path": path.as_os_str().as_encoded_bytes(),
                "version": version,
                "node": node,
            }),
        )
    }
}

/// Sends a request to the daemon at `socket`, or returns `None` when no daemon listens there. A socket served by
/// anyone but the user `owner` gets nothing: requests carry pairing codes and the credential.
pub async fn send(socket: &Path, request: &Request, owner: u32) -> io::Result<Option<Response>> {
    let Some(stream) = connect(socket, owner).await? else {
        return Ok(None);
    };
    let (reader, mut writer) = stream.into_split();
    write_line(&mut writer, request).await?;
    let line = read_line(reader).await?;
    parse(&line).map(Some)
}

/// The daemon's state as [`watch`] receives it, line by line. Dropping it closes the connection.
pub struct Watch {
    reader: BufReader<OwnedReadHalf>,
    _writer: OwnedWriteHalf,
}

impl Watch {
    /// The next line the daemon sent, or `None` once it closed the connection.
    pub async fn next(&mut self) -> io::Result<Option<Response>> {
        let mut line = String::new();
        if (&mut self.reader)
            .take(MAX_LINE)
            .read_line(&mut line)
            .await?
            == 0
        {
            return Ok(None);
        }
        parse(&line).map(Some)
    }
}

/// Sends [`Request::Watch`] to the daemon at `socket` as [`send`] does, or returns `None` when no daemon listens there.
/// The first line follows at once, and another after each change of the status or the questions.
pub async fn watch(socket: &Path, owner: u32) -> io::Result<Option<Watch>> {
    let Some(stream) = connect(socket, owner).await? else {
        return Ok(None);
    };
    let (reader, mut writer) = stream.into_split();
    write_json(&mut writer, &Request::Watch).await?;
    Ok(Some(Watch {
        reader: BufReader::new(reader),
        _writer: writer,
    }))
}

fn parse(line: &str) -> io::Result<Response> {
    serde_json::from_str(line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

async fn connect(socket: &Path, owner: u32) -> io::Result<Option<UnixStream>> {
    let stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let peer = stream.peer_cred()?.uid();
    if peer != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("it is served by another user (uid {peer})"),
        ));
    }
    Ok(Some(stream))
}

/// Runs a request in the daemon, or returns `None` when no daemon answers on the profile's socket.
pub async fn ask(paths: &Paths, request: &Request) -> Result<Option<Response>, String> {
    let socket = paths.socket();
    send(&socket, request, rustix::process::getuid().as_raw())
        .await
        .map_err(|error| format!("the daemon's socket {} failed: {error}", socket.display()))
}

/// Reads one line of at most the socket's line limit.
pub async fn read_line(reader: impl tokio::io::AsyncRead + Unpin) -> io::Result<String> {
    let mut line = String::new();
    BufReader::new(reader.take(MAX_LINE))
        .read_line(&mut line)
        .await?;
    Ok(line)
}

/// Writes `value` as one JSON line and shuts the writer down.
pub async fn write_line(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &impl Serialize,
) -> io::Result<()> {
    write_json(writer, value).await?;
    writer.shutdown().await
}

/// Writes `value` as one JSON line.
pub async fn write_json(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &impl Serialize,
) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await
}

#[cfg(test)]
mod tests {
    use koidrive_driver::{RestoreRequest, Update};
    use tokio::net::UnixListener;

    use super::*;

    #[tokio::test]
    async fn signed_out_writes_non_utf8_cleanup_paths_as_lossy_json_strings() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let bytes = b"/tmp/cleanup-\xff\n\x1b[31m";
        let response = Response::SignedOut {
            not_removed: vec![CleanupFailure {
                path: PathBuf::from(OsString::from_vec(bytes.to_vec())),
                error: "denied\n\x1b[31m".into(),
            }],
        };
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        write_line(&mut writer, &response)
            .await
            .expect("a signed-out response must encode a non-UTF-8 cleanup path");
        let line = read_line(&mut reader).await.unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        assert!(!line.contains('\x1b'));
        let Response::SignedOut { not_removed } = parse(&line).unwrap() else {
            panic!("the cleanup result must survive IPC");
        };
        assert_eq!(
            not_removed[0].path,
            PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
        );
        assert_eq!(not_removed[0].error, "denied\n\x1b[31m");
        let Response::SignedOut { not_removed } = response else {
            unreachable!();
        };
        assert_eq!(not_removed[0].path.as_os_str().as_bytes(), bytes);
    }

    #[test]
    fn an_approved_link_is_not_a_plain_link_to_an_older_daemon() {
        #[derive(Debug, Deserialize)]
        #[serde(tag = "request", rename_all = "snake_case")]
        enum OlderRequest {
            Link { folder: PathBuf, space: String },
        }
        let folder = tempfile::tempdir().unwrap();
        let plain = Request::Link {
            folder: folder.path().into(),
            space: "s1".into(),
        };
        let json = serde_json::to_string(&plain).unwrap();
        let OlderRequest::Link {
            folder: path,
            space,
        } = serde_json::from_str(&json).unwrap();
        assert_eq!(path, folder.path());
        assert_eq!(space, "s1");
        let approved = Request::LinkApproved {
            folder: folder.path().into(),
            space,
            approval: LinkApproval {
                directory: koidrive_driver::DirectoryIdentity::of(
                    &folder.path().metadata().unwrap(),
                ),
                accept_existing_files: false,
            },
        };
        let json = serde_json::to_string(&approved).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), approved);
        assert!(serde_json::from_str::<OlderRequest>(&json).is_err());
    }

    #[test]
    fn an_error_from_an_older_daemon_reads_as_unclassified() {
        let response: Response =
            serde_json::from_str(r#"{"response":"error","message":"it failed"}"#).unwrap();
        assert_eq!(
            response,
            Response::Error {
                message: "it failed".into(),
                code: Code::Failed,
                reason: None,
                next: Vec::new(),
            }
        );
        let response: Response = serde_json::from_str(
            r#"{"response":"error","message":"it failed","code":"usage","reason":"a_reason_from_later"}"#,
        )
        .unwrap();
        assert_eq!(
            response,
            Response::Error {
                message: "it failed".into(),
                code: Code::Usage,
                reason: None,
                next: Vec::new(),
            }
        );
    }

    #[test]
    fn a_status_from_a_daemon_without_pause_or_version_reads_as_unpaused_and_unversioned() {
        let line = r#"{"response":"status","status":{"account":null,"running":true,"folders":[]}}"#;
        assert_eq!(
            serde_json::from_str::<Response>(line).unwrap(),
            Response::Status {
                status: Status {
                    running: true,
                    ..Status::default()
                }
            }
        );
    }

    #[tokio::test]
    async fn a_watch_reads_each_line_the_daemon_sends_until_it_closes() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let daemon = UnixListener::bind(&socket).unwrap();
        let served = tokio::spawn(async move {
            let (stream, _) = daemon.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            for paused in [false, true] {
                let status = Status {
                    paused,
                    ..Status::default()
                };
                let questions = Vec::new();
                write_json(&mut writer, &Response::State { status, questions })
                    .await
                    .unwrap();
            }
            request
        });
        let owner = rustix::process::getuid().as_raw();
        let mut lines = watch(&socket, owner).await.unwrap().unwrap();
        let paused = |response: Option<Response>| match response {
            Some(Response::State { status, .. }) => status.paused,
            other => panic!("{other:?}"),
        };
        assert!(!paused(lines.next().await.unwrap()));
        assert!(paused(lines.next().await.unwrap()));
        assert_eq!(served.await.unwrap(), "{\"request\":\"watch\"}\n");
        assert_eq!(lines.next().await.unwrap(), None);
        assert!(
            watch(&dir.path().join("none.sock"), owner)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_socket_served_by_another_user_gets_no_request() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let daemon = UnixListener::bind(&socket).unwrap();
        let received = tokio::spawn(async move {
            let (stream, _) = daemon.accept().await.unwrap();
            read_line(stream).await.unwrap()
        });
        let someone_else = rustix::process::getuid().as_raw() + 1;
        let request = Request::Pair {
            server: "https://example.com".into(),
            code: "AAAA-BBBB".into(),
            name: "laptop".into(),
        };
        let error = send(&socket, &request, someone_else).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("another user"), "{error}");
        assert_eq!(received.await.unwrap(), "");
    }

    #[test]
    fn recovery_causes_survive_direct_and_watch_responses_with_legacy_defaults() {
        for payload in [
            json!(null),
            json!({ "trigger": "space_rewound", "restore_point_ms": 1_794_442_424_000_i64 }),
            json!({ "trigger": "future_cause" }),
            json!({}),
        ] {
            let mut question = json!({
                "id": "q1", "space_id": "s1", "asked_at_ms": 7,
                "kind": { "type": "recovery", "group": "created_after_restore_point" },
                "items": []
            });
            if !payload.is_null() {
                question["recovery_cause"] = payload.clone();
            }
            let expected: Question = serde_json::from_value(question.clone()).unwrap();
            assert_eq!(expected.recovery_cause.is_some(), !payload.is_null());
            if payload["trigger"] == "future_cause" || payload == json!({}) {
                assert_eq!(
                    expected.recovery_cause.as_ref().unwrap().trigger,
                    koidrive_driver::RecoveryTrigger::Unknown
                );
            }
            for response in [
                json!({ "response": "questions", "questions": [question.clone()] }),
                json!({ "response": "answered_question", "question": question.clone() }),
                json!({ "response": "state", "status": Status::default(), "questions": [question.clone()] }),
            ] {
                let response: Response = serde_json::from_value(response).unwrap();
                let round_trip: Response =
                    serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
                assert_eq!(round_trip, response);
                let question = match round_trip {
                    Response::Questions { questions } | Response::State { questions, .. } => {
                        questions.into_iter().next().unwrap()
                    }
                    Response::AnsweredQuestion { question } => question.unwrap(),
                    _ => unreachable!(),
                };
                assert_eq!(question, expected);
            }
        }
    }

    #[test]
    fn requests_are_tagged_json() {
        let request = Request::Answer {
            question_id: "q1".into(),
            answer: Answer::Restore,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"request":"answer","question_id":"q1","answer":"restore"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);

        let request = Request::Restore {
            space_id: "s1".into(),
            restore: RestoreRequest::Version {
                node_id: "n1".into(),
                version_id: "v1".into(),
                saved_at_ms: Some(1_000),
            },
            folder: "/home/ada/Docs".into(),
            path: "a.txt".into(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"request":"restore","space_id":"s1","restore":{"type":"version","node_id":"n1","version_id":"v1","saved_at_ms":1000},"folder":"/home/ada/Docs","path":"a.txt"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
    }

    #[test]
    fn the_wire_format_is_pinned() {
        fn pinned<T>(value: T, json: &str)
        where
            T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
        {
            assert_eq!(serde_json::to_string(&value).unwrap(), json);
            assert_eq!(serde_json::from_str::<T>(json).unwrap(), value);
        }
        pinned(Request::Status, r#"{"request":"status"}"#);
        pinned(
            Request::Pair {
                server: "https://files.example.com".into(),
                code: "AAAA-BBBB".into(),
                name: "laptop".into(),
            },
            r#"{"request":"pair","server":"https://files.example.com","code":"AAAA-BBBB","name":"laptop"}"#,
        );
        pinned(
            Request::SaveCredential {
                credential: Credential {
                    server: "https://files.example.com".into(),
                    user_id: "u1".into(),
                    user_name: None,
                    device_id: "d1".into(),
                    device_name: "laptop".into(),
                    refresh_credential: "secret".into(),
                },
            },
            r#"{"request":"save_credential","credential":{"server":"https://files.example.com","user_id":"u1","device_id":"d1","device_name":"laptop","refresh_credential":"secret"}}"#,
        );
        pinned(
            Request::Lift {
                folder: "/home/ada/Docs".into(),
                replan: true,
            },
            r#"{"request":"lift","folder":"/home/ada/Docs","replan":true}"#,
        );
        pinned(
            Request::Restart {
                folder: "/a".into(),
            }
            .once(
                Some(RequestId::parse("r1").unwrap()),
                Key::restart(Path::new("/a")),
            ),
            r#"{"request":"once","request_id":"r1","key":{"command":"folder restart","arguments":{"folder":[47,97]}},"run":{"request":"restart","folder":"/a"}}"#,
        );
        pinned(Request::Access, r#"{"request":"access"}"#);
        pinned(Request::Pause, r#"{"request":"pause"}"#);
        pinned(Request::Resume, r#"{"request":"resume"}"#);
        pinned(Request::Watch, r#"{"request":"watch"}"#);
        pinned(
            Request::Leave {
                confirm_last_device: false,
            },
            r#"{"request":"leave","confirm_last_device":false}"#,
        );
        pinned(
            Response::SignedOut {
                not_removed: Vec::new(),
            },
            r#"{"response":"signed_out"}"#,
        );
        pinned(
            Response::SignedOut {
                not_removed: vec![CleanupFailure {
                    path: "/a/.koidrive/device.json".into(),
                    error: "Permission denied (os error 13)".into(),
                }],
            },
            r#"{"response":"signed_out","not_removed":[{"path":"/a/.koidrive/device.json","error":"Permission denied (os error 13)"}]}"#,
        );

        pinned(
            Response::Status {
                status: Status {
                    account: Some(Account {
                        server: "https://files.example.com".into(),
                        user_id: "u1".into(),
                        user_name: Some("Ada".into()),
                        device_id: "d1".into(),
                        device_name: "laptop".into(),
                    }),
                    running: true,
                    paused: true,
                    version: Some("0.1.0-alpha.2".into()),
                    update: Update::Available {
                        latest: "0.1.2".into(),
                        url: "https://github.com/koidrive/koidrive/releases/tag/v0.1.2".into(),
                        checked_at_ms: 1_763_000_000_000,
                    },
                    folders: Vec::new(),
                    progress: None,
                },
            },
            r#"{"response":"status","status":{"account":{"server":"https://files.example.com","user_id":"u1","user_name":"Ada","device_id":"d1","device_name":"laptop"},"running":true,"paused":true,"version":"0.1.0-alpha.2","update":{"state":"available","latest":"0.1.2","url":"https://github.com/koidrive/koidrive/releases/tag/v0.1.2","checked_at_ms":1763000000000},"folders":[]}}"#,
        );
        pinned(Response::Paused, r#"{"response":"paused"}"#);
        pinned(Response::AlreadyPaused, r#"{"response":"already_paused"}"#);
        pinned(Response::Resumed, r#"{"response":"resumed"}"#);
        pinned(Response::NotPaused, r#"{"response":"not_paused"}"#);
        pinned(
            Response::State {
                status: Status::default(),
                questions: Vec::new(),
            },
            r#"{"response":"state","status":{"account":null,"running":false,"paused":false,"version":null,"update":{"state":"unknown"},"folders":[]},"questions":[]}"#,
        );
        pinned(
            Response::Unlinked {
                space_id: "s1".into(),
                path: "/home/ada/Docs".into(),
            },
            r#"{"response":"unlinked","space_id":"s1","path":"/home/ada/Docs"}"#,
        );
        pinned(
            Response::AnsweredQuestion { question: None },
            r#"{"response":"answered_question","question":null}"#,
        );
        pinned(
            Response::AlreadyAnswered,
            r#"{"response":"already_answered"}"#,
        );
        pinned(
            Response::Access {
                blocked: vec![Blocked {
                    folder: "/home/ada/Documents".into(),
                    waiting: true,
                }],
            },
            r#"{"response":"access","blocked":[{"folder":"/home/ada/Documents","waiting":true}]}"#,
        );
        pinned(
            Response::Error {
                message: "no unanswered question has the id \"q1\"".into(),
                code: Code::StaleDecision,
                reason: None,
                next: vec![Next::of(&["koi", "resolve", "--json"])],
            },
            r#"{"response":"error","message":"no unanswered question has the id \"q1\"","code":"stale_decision","next":[{"argv":["koi","resolve","--json"]}]}"#,
        );
        pinned(
            Response::Error {
                message: "it failed".into(),
                code: Code::Failed,
                reason: None,
                next: Vec::new(),
            },
            r#"{"response":"error","message":"it failed","code":"failed"}"#,
        );
        pinned(
            Response::Error {
                message: "a link's password has 8 to 256 characters".into(),
                code: Code::Usage,
                reason: Some(Reason::PasswordLength),
                next: Vec::new(),
            },
            r#"{"response":"error","message":"a link's password has 8 to 256 characters","code":"usage","reason":"password_length"}"#,
        );
    }

    #[test]
    fn every_code_has_its_exit_status_and_only_unavailable_is_retryable() {
        let all = [
            (Code::Failed, 1, "failed"),
            (Code::Usage, 2, "usage"),
            (Code::DecisionRequired, 3, "decision_required"),
            (Code::AuthRequired, 4, "auth_required"),
            (Code::Halted, 5, "halted"),
            (Code::Unavailable, 6, "unavailable"),
            (Code::StaleDecision, 7, "stale_decision"),
            (Code::Interrupted, 130, "interrupted"),
        ];
        for (code, exit, name) in all {
            assert_eq!(code.exit(), exit, "{code:?}");
            assert_eq!(serde_json::to_value(code).unwrap(), name);
            assert_eq!(code.retryable(), code == Code::Unavailable, "{code:?}");
        }
    }

    #[test]
    fn a_request_id_is_a_file_name() {
        for good in [
            "r1",
            "019b2c4e-7d1f-7000-8000-000000000001",
            "deploy_2",
            &"a".repeat(128),
        ] {
            assert!(RequestId::parse(good).is_ok(), "{good}");
        }
        for bad in ["", "..", "a/b", "a.json", "r 1", "ré", &"a".repeat(129)] {
            assert!(RequestId::parse(bad).is_err(), "{bad:?}");
        }
        assert!(serde_json::from_value::<RequestId>(json!("../x")).is_err());
    }
}
