// SPDX-License-Identifier: Apache-2.0

use crate::args::{Cli, Command};
use crate::daemon::daemon_driver;
use crate::device::{
    DeviceAction, Invitation, device, device_id, device_table, devices, invited, left, refused,
    replayed,
};
use crate::error::CliError;
use crate::folder::restarted;
use crate::formatting::{ago, now_ms, size};
use crate::lifecycle::{finish_lifecycle, finish_setup, run};
use crate::logging::log_ansi;
use crate::options::RequestArg;
use crate::profile::{reading, signed_in};
use crate::report::{DeviceData, Done, Envelope, UpdateNoticeData, UsageData};
use crate::resolve::{
    ITEMS_SHOWN, answer_to, banner, question_brief, question_notice, question_text,
};
use crate::space::{SpaceAction, space, space_table, usage, usage_done, usage_report};
use crate::status::{folder_text, status_summary, status_text};
use crate::{
    app, command_path, history, ipc, json_requested, report, schema, service, setup, terminal,
};
use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};
use koidrive_driver::{
    Answer, Credential, Driver, DriverError, FolderState, FolderStatus, Limit, Paths, Question,
    QuestionKind, RecoveryGroup, Status,
};
use koidrive_ipc::{Code, Request, RequestId, Response};
use koidrive_net::Auth;
use koidrive_proto::v1::{DeviceInfo, SpaceInfo, SpaceUsage};
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[test]
fn concise_halted_status_gives_a_restart_after_commandless_problems() {
    for problem in [
        "the folder cannot be read: Permission denied (os error 13)",
        "the folder cannot be claimed",
        "a file cannot be placed in the folder",
    ] {
        let status: Status = serde_json::from_value(serde_json::json!({
            "account":null, "running":true, "folders":[{
                "space_id":"s1", "space_name":"Work", "path":"/home/ada/Work Files", "state":"halted",
                "history_id":null,"last_seq":null,"head_seq":null,"remote_nodes":0,"local_entries":null,"questions":0,
                "problem":problem
            }]
        })).unwrap();
        let service: service::Report = serde_json::from_value(
            serde_json::json!({"manager":"launchd", "name":"fixture", "state":"off"}),
        )
        .unwrap();
        let notice = UpdateNoticeData::of(&status.update, "0.1.0", None);
        let summary = status_summary(&status, &[], &service, &notice);
        assert!(summary.contains(problem), "{summary}");
        assert!(
            summary.contains(
                "Fix the problem above, then run koi folder restart '/home/ada/Work Files'"
            ),
            "{summary}"
        );
    }
}

#[test]
fn concise_recovery_keeps_causes_in_separate_sentences() {
    for trigger in ["store_rolled_back", "store_older_than_disk"] {
        let status: Status = serde_json::from_value(serde_json::json!({
            "account":null, "running":true, "folders":[{
                "space_id":"s1", "space_name":"Work", "path":"/home/ada/Work", "state":"catching_up",
                "history_id":null,"last_seq":null,"head_seq":null,"remote_nodes":0,"local_entries":null,"questions":0,
                "problem":null,"recovery":{"trigger":trigger}
            }]
        })).unwrap();
        let service: service::Report = serde_json::from_value(
            serde_json::json!({"manager":"launchd", "name":"fixture", "state":"off"}),
        )
        .unwrap();
        let notice = UpdateNoticeData::of(&status.update, "0.1.0", None);
        let summary = status_summary(&status, &[], &service, &notice);
        assert!(summary.contains(". Recovering sync."), "{summary}");
        assert!(!summary.contains("server Recovering"), "{summary}");
        assert!(!summary.contains("disk Recovering"), "{summary}");
    }
}

#[test]
fn concise_halted_status_keeps_the_actual_recovery_action() {
    let status: Status = serde_json::from_value(serde_json::json!({
        "account":null, "running":true, "folders":[{
            "space_id":"s1", "space_name":"Work", "path":"/home/ada/Work", "state":"halted",
            "history_id":null,"last_seq":null,"head_seq":null,"remote_nodes":0,"local_entries":null,"questions":0,
            "problem":"Fix file access, then run koi folder restart /home/ada/Work"
        }]
    })).unwrap();
    let service: service::Report = serde_json::from_value(
        serde_json::json!({"manager":"launchd", "name":"fixture", "state":"off"}),
    )
    .unwrap();
    let notice = UpdateNoticeData::of(&status.update, "0.1.0", None);
    let summary = status_summary(&status, &[], &service, &notice);
    assert!(summary.contains("koi folder restart"));
    assert!(!summary.contains("koi diagnose"), "{summary}");
}

#[test]
fn global_verbose_keeps_the_update_commands_detail_option() {
    for args in [
        ["koi", "update", "--verbose"],
        ["koi", "--verbose", "update"],
    ] {
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Update { verbose: true })
        ));
    }
}

#[test]
fn concise_status_explains_a_quota_wait_before_a_problem_is_recorded() {
    let status: Status = serde_json::from_value(serde_json::json!({
        "account":null, "running":true, "folders":[{
            "space_id":"s1", "space_name":"Work", "path":"/home/ada/Work", "state":"up_to_date",
            "history_id":null,"last_seq":null,"head_seq":null,"remote_nodes":0,"local_entries":null,"questions":0,
            "problem":null,"wait":{"kind":"quota", "until_ms":now_ms()+60000}
        }]
    })).unwrap();
    let service: service::Report = serde_json::from_value(
        serde_json::json!({"manager":"launchd", "name":"fixture", "state":"off"}),
    )
    .unwrap();
    let notice = UpdateNoticeData::of(&status.update, "0.1.0", None);
    let summary = status_summary(&status, &[], &service, &notice);
    assert!(
        summary.contains("Storage is full or a daily limit was reached"),
        "{summary}"
    );
    assert!(summary.contains("Retry"), "{summary}");
}

#[test]
fn concise_conflicts_name_their_writers_and_this_device_without_writer_ids() {
    let question = conflict("docs/notes (conflict).txt");
    let status: Status = serde_json::from_value(serde_json::json!({
        "account":{"server":"https://files.example.com/", "user_id":"owner", "device_id":DESK,"device_name":"Desktop"},
        "running":true, "folders":[{
            "space_id":question.space_id, "space_name":"Work", "path":"/home/ada/Work", "state":"up_to_date",
            "history_id":null,"last_seq":null,"head_seq":null,"remote_nodes":0,"local_entries":null,"questions":1,"problem":null
        }]
    })).unwrap();
    let service: service::Report = serde_json::from_value(
        serde_json::json!({"manager":"launchd", "name":"fixture", "state":"off"}),
    )
    .unwrap();
    let notice = UpdateNoticeData::of(&status.update, "0.1.0", None);
    let summary = status_summary(&status, &[question], &service, &notice);
    assert!(
        summary.contains("Original: docs/notes.txt, written by Ada's Desktop (this device)\n"),
        "{summary}"
    );
    assert!(
        summary.contains("Conflict copy: 'docs/notes (conflict).txt', written by agent-box\n"),
        "{summary}"
    );
    assert!(!summary.contains(DESK), "{summary}");
    assert!(!summary.contains(LAPTOP), "{summary}");
    assert!(!summary.contains("space s1"), "{summary}");
    assert!(summary.contains("keep-node:"));
}
use koidrive_net::ApiError;
use koidrive_proto::v1::device_info::Kind;

const ID: &str = "0190a0b0-0000-7000-8000-000000000001";

fn parse(args: &[&str]) -> Result<Option<Command>, clap::Error> {
    Cli::try_parse_from(args).map(|cli| cli.command)
}

/// Answers one request per entry of `replies`, a status and a body, on a loopback port, in order, and returns the
/// server's URL and, once all are answered, the path of each request.
async fn serve(replies: Vec<(u16, Vec<u8>)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let mut paths = Vec::new();
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let read = socket.read(&mut request).await.unwrap();
            let line = String::from_utf8_lossy(&request[..read])
                .lines()
                .next()
                .unwrap()
                .to_owned();
            paths.push(line.split(' ').nth(1).unwrap().to_owned());
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        }
        paths
    });
    (server, served)
}

fn token() -> (u16, Vec<u8>) {
    let token = koidrive_proto::v1::AccessToken {
        token: "token".into(),
        expires_at_ms: i64::MAX,
        ..Default::default()
    };
    (200, prost::Message::encode_to_vec(&token))
}

/// Saves a credential of `device_id` for `server` in `paths`, in place of any other.
fn signed_in_as(paths: &Paths, server: &str, device_id: &str) {
    let _ = std::fs::remove_file(paths.credential());
    Credential {
        server: server.into(),
        user_id: "u1".into(),
        user_name: None,
        device_id: device_id.into(),
        device_name: "vm".into(),
        refresh_credential: "secret".into(),
    }
    .save_new(&paths.credential())
    .unwrap();
}

#[tokio::test]
async fn the_device_list_stops_at_a_cursor_it_has_seen() {
    let page = |cursor: &str| {
        let page = koidrive_proto::v1::DeviceList {
            devices: vec![DeviceInfo {
                device_id: cursor.into(),
                ..Default::default()
            }],
            next_cursor: cursor.into(),
        };
        (200, prost::Message::encode_to_vec(&page))
    };
    let (server, served) = serve(vec![token(), page("a"), page("b"), page("a")]).await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    signed_in_as(&paths, &server, "d1");
    let auth = signed_in(&paths).unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), devices(&auth))
        .await
        .expect("the listing ended");
    assert_eq!(outcome.unwrap_err().code, Code::Failed);
    assert_eq!(served.await.unwrap().len(), 4);
}

#[tokio::test]
async fn a_space_another_run_created_first_is_unchanged() {
    let list = |spaces: Vec<SpaceInfo>| {
        let list = koidrive_proto::v1::SpaceList {
            spaces,
            ..Default::default()
        };
        (200, prost::Message::encode_to_vec(&list))
    };
    let taken = koidrive_proto::v1::Error {
        code: koidrive_proto::v1::error::Code::InvalidArgument as i32,
        message: "you already have a space with this name".into(),
        ..Default::default()
    };
    let notes = SpaceInfo {
        space_id: ID.into(),
        name: "Notes".into(),
        ..Default::default()
    };
    let (server, served) = serve(vec![
        token(),
        list(Vec::new()),
        (409, prost::Message::encode_to_vec(&taken)),
        list(vec![notes]),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    signed_in_as(&paths, &server, "d1");
    let create = SpaceAction::Create {
        name: "Notes".into(),
    };
    let done = space(paths, create).await.unwrap();
    assert_eq!(done.outcome, crate::report::Outcome::Unchanged);
    assert_eq!(done.text, format!("{ID}\n"));
    assert_eq!(
        served.await.unwrap(),
        ["/v1/token", "/v1/spaces", "/v1/spaces", "/v1/spaces"]
    );
}

#[tokio::test]
async fn a_replayed_invitation_belongs_to_the_device_that_asked_for_it() {
    let started = koidrive_proto::v1::PairStartResponse {
        code: "K7QM-2XPD".into(),
        expires_at_ms: i64::MAX,
    };
    let started = (200, prost::Message::encode_to_vec(&started));
    let (server, served) = serve(vec![token(), started]).await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let invite = || {
        device(
            paths.clone(),
            DeviceAction::Invite {
                request: RequestArg {
                    request_id: Some(RequestId::parse("r1").unwrap()),
                },
            },
        )
    };
    signed_in_as(&paths, &server, "d1");
    let first = invite().await.unwrap();
    assert_eq!(invite().await.unwrap().text, first.text);
    signed_in_as(&paths, &server, "d2");
    let other = invite().await.unwrap_err();
    assert_eq!(other.code, Code::StaleDecision, "{other:?}");
    assert_eq!(served.await.unwrap(), ["/v1/token", "/v1/pair/start"]);
}

#[test]
fn a_replayed_invitation_shows_the_first_code_until_it_expires() {
    let invitation = Invitation {
        code: "K7QM-2XPD".into(),
        server: "https://files.example.com/".into(),
        expires_at_ms: 600_000,
    };
    let id = RequestId::parse("r1").unwrap();
    let first = invited(&invitation, 0).unwrap();
    assert_eq!(first.text, "K7QM-2XPD@files.example.com\n");
    let again = replayed(&id, &invitation, 599_999).unwrap();
    assert_eq!(
        (again.outcome, &again.data, &again.text),
        (first.outcome, &first.data, &first.text)
    );
    let expired = replayed(&id, &invitation, 600_000).unwrap_err();
    assert_eq!(expired.code, Code::StaleDecision);
    assert_eq!(
        expired.message,
        "stale request: the pairing code of the request r1 expired; run koi device invite with a new request ID"
    );
    assert!(expired.next.is_empty());
}

#[test]
fn only_a_refusal_the_server_sent_counts_as_no_change() {
    let refusal = ApiError::Server {
        status: 429,
        error: koidrive_net::ServerError(koidrive_proto::v1::Error::default()),
    };
    assert!(refused(&refusal));
    assert!(refused(&ApiError::Status { status: 403 }));
    assert!(refused(&ApiError::Unreadable { status: 400 }));
    let transport = reqwest::Client::new().get("not a url").build().unwrap_err();
    let internal = koidrive_proto::v1::Error {
        code: koidrive_proto::v1::error::Code::Internal as i32,
        ..Default::default()
    };
    for unknown in [
        ApiError::Server {
            status: 500,
            error: koidrive_net::ServerError(internal),
        },
        ApiError::Transport(transport),
        ApiError::Status { status: 504 },
        ApiError::Unreadable { status: 502 },
        ApiError::Decode("PairStartResponse"),
        ApiError::TooLarge { limit: 1 },
    ] {
        assert!(!refused(&unknown), "{unknown:?}");
    }
}

#[test]
fn request_ids_are_checked_where_they_are_parsed() {
    assert!(parse(&["koi", "folder", "restart", "/a", "--request-id", "r-1_a"]).is_ok());
    assert!(parse(&["koi", "device", "invite", "--request-id", "r1"]).is_ok());
    assert!(parse(&["koi", "history", "restore", "a.txt", "--request-id", "r1"]).is_ok());
    let error = parse(&[
        "koi",
        "folder",
        "retry",
        "/a",
        "--resend",
        "--request-id",
        "../r1",
    ])
    .err()
    .unwrap();
    assert_eq!(error.kind(), ErrorKind::ValueValidation);
}

#[test]
fn the_status_text_escapes_every_name_path_and_problem() {
    let folder = FolderStatus {
        recovery: None,
        space_id: "s\u{1b}[2J".into(),
        space_name: "Home\u{202e}gpj.exe".into(),
        path: "/home/ada/\u{1b}]0;owned\u{7}Docs\nFake: up to date".into(),
        state: FolderState::Halted,
        history_id: None,
        last_seq: None,
        head_seq: None,
        remote_nodes: 0,
        local_entries: None,
        questions: 0,
        problem: Some("cannot read \u{1b}[31mred\u{9b}31m".into()),
        progress: None,
        wait: None,
    };
    let status = Status {
        account: Some(koidrive_driver::Account {
            server: "https://files.example.com/\u{1b}[1A".into(),
            user_id: "u1".into(),
            user_name: None,
            device_id: "d\r1".into(),
            device_name: "Ada\u{1b}[8m".into(),
        }),
        running: true,
        folders: vec![folder],
        ..Status::default()
    };
    let notice = UpdateNoticeData::of(&status.update, "0.1.0", Some("koi update"));
    let text = status_text(&status, &[], "", &notice);
    for raw in ['\u{1b}', '\u{7}', '\u{9b}', '\u{202e}', '\r'] {
        assert!(
            !text.contains(raw),
            "{raw:?} reached the terminal: {text:?}"
        );
    }
    assert_eq!(text.lines().count(), 4, "{text}");
    assert!(text.contains("Ada\\u{1b}[8m"), "{text}");
    assert!(text.contains("Docs\\u{a}Fake"), "{text}");
    assert!(text.contains("red\\u{9b}31m"), "{text}");
}

#[test]
fn the_status_text_names_the_rewind_a_folder_recovers_from() {
    let folder = FolderStatus {
        recovery: Some(koidrive_engine::question::RecoveryCause::rewind(
            7,
            1_794_442_424_000,
        )),
        progress: None,
        space_id: "s1".into(),
        space_name: "Home".into(),
        path: "/home/ada/Docs".into(),
        state: FolderState::UpToDate,
        history_id: None,
        last_seq: None,
        head_seq: None,
        remote_nodes: 0,
        local_entries: None,
        questions: 1,
        problem: None,
        wait: None,
    };
    assert!(
        folder_text(&folder).ends_with(
            "\n  the space was rewound on the server to seq 7 (requested 2026-11-12 00:13 UTC)\n"
        ),
        "{}",
        folder_text(&folder)
    );
}

#[test]
fn the_status_text_names_the_rewind_beside_a_problem_and_only_once() {
    let cause = koidrive_engine::question::RecoveryCause::rewind(7, 1_794_442_424_000);
    let rewound = "the space was rewound on the server to seq 7 (requested 2026-11-12 00:13 UTC)";
    let folder = |problem: &str| FolderStatus {
        recovery: Some(cause),
        progress: None,
        space_id: "s1".into(),
        space_name: "Home".into(),
        path: "/home/ada/Docs".into(),
        state: FolderState::UpToDate,
        history_id: None,
        last_seq: None,
        head_seq: None,
        remote_nodes: 0,
        local_entries: None,
        questions: 1,
        problem: Some(problem.into()),
        wait: None,
    };
    let text = folder_text(&folder("a.txt waits to be read again"));
    assert!(
        text.ends_with(&format!("\n  a.txt waits to be read again\n  {rewound}\n")),
        "{text}"
    );
    let text = folder_text(&folder(rewound));
    assert_eq!(text.matches(rewound).count(), 1, "{text}");
}

#[test]
fn logs_carry_colours_only_on_a_terminal_and_never_under_json() {
    assert!(log_ansi(true, false));
    assert!(!log_ansi(true, true));
    assert!(!log_ansi(false, false));
    assert!(!log_ansi(false, true));
}

#[test]
fn the_restart_report_escapes_the_folder_and_its_problem() {
    let folder = |state, problem: Option<&str>| FolderStatus {
        recovery: None,
        space_id: "s1".into(),
        space_name: "Home".into(),
        path: "/home/ada/Docs\u{1b}[31m\nFORGED".into(),
        state,
        history_id: None,
        last_seq: None,
        head_seq: None,
        remote_nodes: 0,
        local_entries: None,
        questions: 0,
        problem: problem.map(str::to_owned),
        progress: None,
        wait: None,
    };
    for state in [FolderState::Stopped, FolderState::CatchingUp] {
        let text = restarted(folder(state, None)).unwrap().text;
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert_eq!(text.lines().count(), 1, "{text:?}");
        assert!(text.contains("Docs\\u{1b}[31m\\u{a}FORGED"), "{text:?}");
    }
    let error = restarted(folder(FolderState::Halted, Some("gone\n\u{7}"))).unwrap_err();
    assert_eq!(error.code, Code::Halted);
    assert!(
        !error.message.contains(['\u{1b}', '\n', '\u{7}']),
        "{:?}",
        error.message
    );
}

#[test]
fn codes_from_stdin_replace_but_never_accompany_positional_codes() {
    for prefix in [
        vec!["koi", "join"],
        vec![
            "koi",
            "server",
            "claim",
            "files.example.com",
            "--user",
            "Ada",
        ],
    ] {
        let mut args = prefix.clone();
        args.push("--code-stdin");
        assert!(parse(&args).is_ok(), "{args:?}");
        args.push("ABCD-EFGH");
        assert_eq!(
            parse(&args).err().unwrap().kind(),
            ErrorKind::ArgumentConflict,
            "{args:?}"
        );
        assert_eq!(
            parse(&prefix).err().unwrap().kind(),
            ErrorKind::MissingRequiredArgument,
            "{prefix:?}"
        );
        let mut positional = prefix;
        positional.push("ABCD-EFGH");
        assert!(parse(&positional).is_ok(), "{positional:?}");
    }
}

#[tokio::test]
async fn leave_cleanup_quotes_terminal_controls_but_preserves_json_data() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let path = "/cleanup/line\n\u{1b}[2J";
    let error = "denied\n\u{1b}[31m";
    let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
    let served = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        koidrive_ipc::read_line(&mut stream).await.unwrap();
        koidrive_ipc::write_line(
            &mut stream,
            &Response::SignedOut {
                not_removed: vec![koidrive_driver::CleanupFailure {
                    path: path.into(),
                    error: error.into(),
                }],
            },
        )
        .await
        .unwrap();
    });

    let done = device(paths, DeviceAction::Leave { last_device: false })
        .await
        .unwrap();
    served.await.unwrap();

    assert!(done.text.contains("\"/cleanup/line\\n\\u{1b}[2J\""));
    assert!(done.text.contains("denied\\u{a}\\u{1b}[31m"));
    assert_eq!(done.text.lines().count(), 2);
    assert!(!done.text.contains('\u{1b}'));
    assert_eq!(done.data["not_removed"][0]["path"], path);
    assert_eq!(done.data["not_removed"][0]["error"], error);
    let encoded = serde_json::to_vec(&Envelope::of("device leave", done)).unwrap();
    let decoded: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded["data"]["not_removed"][0]["path"], path);
    assert_eq!(decoded["data"]["not_removed"][0]["error"], error);
}

#[test]
fn leave_cleanup_accepts_non_utf8_paths_and_emits_lossy_json_without_panicking() {
    use std::os::unix::ffi::OsStringExt;

    let done = left(vec![koidrive_driver::CleanupFailure {
        path: PathBuf::from(OsString::from_vec(b"/cleanup/bad\xff\n\x1b".to_vec())),
        error: "denied\n\u{1b}".into(),
    }]);

    assert!(done.text.contains("\"/cleanup/bad\\xFF\\n\\u{1b}\""));
    assert!(done.text.contains("denied\\u{a}\\u{1b}"));
    assert_eq!(done.text.lines().count(), 2);
    assert!(!done.text.contains('\u{1b}'));
    assert_eq!(
        done.data["not_removed"][0]["path"],
        "/cleanup/bad\u{fffd}\n\u{1b}"
    );
    assert_eq!(done.data["not_removed"][0]["error"], "denied\n\u{1b}");
    let encoded = serde_json::to_vec(&Envelope::of("device leave", done)).unwrap();
    let decoded: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        decoded["data"]["not_removed"][0]["path"],
        "/cleanup/bad\u{fffd}\n\u{1b}"
    );
    assert_eq!(decoded["data"]["not_removed"][0]["error"], "denied\n\u{1b}");
}

#[tokio::test]
async fn leaving_retries_cleanup_without_requiring_a_paired_account() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
    let served = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let line = koidrive_ipc::read_line(&mut stream).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Request>(&line).unwrap(),
            Request::Leave {
                confirm_last_device: false,
            }
        );
        koidrive_ipc::write_line(
            &mut stream,
            &Response::Error {
                code: Code::Failed,
                message: "cleanup pending".into(),
                reason: None,
                human: None,
                next: Vec::new(),
            },
        )
        .await
        .unwrap();
    });
    let error = device(paths, DeviceAction::Leave { last_device: false })
        .await
        .err()
        .unwrap();
    served.await.unwrap();
    assert_eq!(error.message, "cleanup pending");
}

#[tokio::test]
async fn resume_without_a_daemon_reconciles_without_overriding_background_off() {
    let (_dir, paths, _root) = ipc::tests::linked_profile();
    std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    std::fs::write(&paths.config_file, "[service]\nbackground = false\n").unwrap();
    assert_eq!(
        ipc::run(&paths, Request::Pause).await.unwrap(),
        Response::Paused
    );
    assert!(!paths.socket().exists());
    let validator = jsonschema::validator_for(&schema::envelope("resume").unwrap()).unwrap();

    for expected in [report::Outcome::Ok, report::Outcome::Unchanged] {
        let done = run(
            paths.clone(),
            Command::Resume,
            true,
            false,
            Arc::new(terminal::Terminal::new(true, false)),
        )
        .await
        .unwrap();
        assert_eq!(done.outcome, expected);
        assert_eq!(done.data["service"]["state"], "off");
        assert_eq!(done.data["service"]["folders"], 1);
        assert!(!paths.paused().exists());
        assert!(!paths.socket().exists());
        let envelope = serde_json::to_value(report::Envelope::of("resume", done)).unwrap();
        assert!(validator.is_valid(&envelope), "{envelope:#}");
    }
}

#[test]
fn a_daemon_waits_asynchronously_for_a_stopped_profile_reader() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let held = Driver::open(paths.clone()).unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            drop(held);
        });
        let opened = tokio::time::timeout(std::time::Duration::from_secs(2), daemon_driver(paths))
            .await
            .expect("the daemon must yield while a status reader holds the profile");
        assert!(
            opened.is_ok(),
            "the daemon must start after the reader releases the profile"
        );
        release.await.unwrap();
    });
}

#[tokio::test]
async fn a_daemon_does_not_wait_for_another_daemon_that_already_listens() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let _held = Driver::open(paths.clone()).unwrap();
    let _listener = koidrive_ipc::bind(&paths.socket()).unwrap();
    let opened = tokio::time::timeout(std::time::Duration::from_secs(1), daemon_driver(paths))
        .await
        .unwrap();
    assert!(matches!(opened, Err(DriverError::Busy(_))));
}

#[test]
fn setup_uses_the_final_service_handoff_without_changing_reconcile_results() {
    for (initial_state, later_state, app_failed) in [
        ("unavailable", "running", false),
        ("running", "unavailable", true),
    ] {
        for setup_app in [false, true] {
            for has_handoff in [false, true] {
                let report = |state| -> service::Report {
                    serde_json::from_value(serde_json::json!({
                        "manager":"launchd", "name":"fixture", "state":state,
                        "problem": if state == "unavailable" { Some("fixture service failure") } else { None }
                    })).unwrap()
                };
                let initial = report(initial_state);
                let later = report(later_state);
                let selected = if setup_app && has_handoff {
                    &later
                } else {
                    &initial
                };
                let expected_service = serde_json::to_value(selected).unwrap();
                let expected_text = match selected.text() {
                    text if text.is_empty() && !setup_app => selected.status_line(),
                    text => text,
                };
                let app: app::Report = serde_json::from_value(serde_json::json!({
                    "state":"present", "path":"/fixture/Koidrive.app", "version":"99.0.0",
                    "change":"installed", "launch":"not_requested",
                    "problem": app_failed.then_some("fixture app failure")
                }))
                .unwrap();
                let expected_app = serde_json::to_value(&app).unwrap();
                let app_text = app.text();
                let result = finish_lifecycle(
                    Done::ok(serde_json::json!({"device":"fixture"}), "Connected\n"),
                    Some(initial),
                    Some((app, has_handoff.then_some(later))),
                    setup_app,
                    !setup_app,
                );
                let done = if setup_app && app_failed {
                    let error = result.unwrap_err();
                    assert_eq!(error.code, Code::Failed);
                    *error.partial.unwrap()
                } else {
                    result.unwrap()
                };
                assert_eq!(done.data["service"], expected_service);
                assert_eq!(done.data["app"], expected_app);
                assert_eq!(done.data["device"], "fixture");
                assert_eq!(done.text, format!("Connected\n{expected_text}{app_text}"));
            }
        }
    }
}

#[tokio::test]
async fn setup_finish_bounds_a_status_request_that_accepts_but_never_replies() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
    let (accepted, received) = tokio::sync::oneshot::channel();
    let served = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = koidrive_ipc::read_line(&mut stream).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<Request>(&request).unwrap(),
            Request::Status
        ));
        accepted.send(()).unwrap();
        std::future::pending::<()>().await;
        drop(stream);
    });
    let report: service::Report = serde_json::from_value(serde_json::json!({
        "manager":"systemd", "name":"fixture", "state":"running", "lingers":true
    }))
    .unwrap();
    let done = Done {
        note: "Setup complete. Add files to ~/Koidrive.\nCheck sync: koi status\n".into(),
        ..Done::printed()
    };
    let terminal = terminal::Terminal::setup(true, true);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        finish_setup(&paths, done, Some(report), None, &terminal),
    )
    .await;
    served.abort();
    let _ = served.await;
    received
        .await
        .expect("the daemon must have received the final status request");
    let done = result
        .expect("setup must stop waiting for a silent status response")
        .unwrap();
    assert_eq!(
        done.note,
        "Folder linked. Could not confirm background sync.\nCheck sync: koi status\n"
    );
    assert!(!done.note.contains("Setup complete"));
    assert!(!done.note.contains("Background sync is running"));
    assert_eq!(done.data["service"]["state"], "running");
}

#[tokio::test]
async fn setup_finish_reports_pending_decisions_in_active_folders() {
    const FIXTURE: &str = "KOIDRIVE_TEST_SETUP_PENDING";
    let Ok(case) = std::env::var(FIXTURE) else {
        for case in ["connecting", "bootstrapping", "catching_up", "up_to_date"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::setup_finish_reports_pending_decisions_in_active_folders",
                    "--nocapture",
                ])
                .env(FIXTURE, case)
                .output()
                .unwrap();
            let text = String::from_utf8(output.stderr).unwrap();
            assert!(
                output.status.success(),
                "{case}: {text}\n{}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(
                text.contains("Review pending decisions for /fixture/Koidrive: koi resolve"),
                "{case}: {text}"
            );
            assert!(
                !text.contains("Background sync is running"),
                "{case}: {text}"
            );
            assert!(!text.contains("Sync is offline"), "{case}: {text}");
        }
        return;
    };
    let state = match case.as_str() {
        "connecting" => FolderState::Connecting,
        "bootstrapping" => FolderState::Bootstrapping,
        "catching_up" => FolderState::CatchingUp,
        "up_to_date" => FolderState::UpToDate,
        _ => panic!("unknown fixture {case}"),
    };
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
    let served = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = koidrive_ipc::read_line(&mut stream).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<Request>(&request).unwrap(),
            Request::Status
        ));
        let status = Status {
            running: true,
            folders: vec![
                serde_json::from_value(serde_json::json!({
                    "space_id": "s1",
                    "space_name": "Koidrive",
                    "path": "/fixture/Koidrive",
                    "state": state,
                    "history_id": null,
                    "last_seq": null,
                    "head_seq": null,
                    "remote_nodes": 0,
                    "local_entries": null,
                    "questions": 1,
                    "problem": null,
                    "wait": null
                }))
                .unwrap(),
            ],
            ..Status::default()
        };
        koidrive_ipc::write_line(&mut stream, &Response::Status { status })
            .await
            .unwrap();
    });
    let report: service::Report = serde_json::from_value(serde_json::json!({
        "manager":"systemd", "name":"fixture", "state":"running", "lingers":true
    }))
    .unwrap();
    let done = finish_setup(
        &paths,
        Done {
            note: "Setup complete. Add files to ~/Koidrive.\nCheck sync: koi status\n".into(),
            ..Done::printed()
        },
        Some(report),
        None,
        &terminal::Terminal::setup(true, true),
    )
    .await
    .unwrap();
    served.await.unwrap();
    assert_eq!(
        done.note,
        "Folder linked. Sync needs attention.\nCheck sync: koi status\n"
    );
}

#[tokio::test]
async fn setup_finish_preserves_the_linux_logout_warning() {
    const FIXTURE: &str = "KOIDRIVE_TEST_SETUP_LINGER";
    let Ok(case) = std::env::var(FIXTURE) else {
        for (case, warns) in [
            ("systemd-off", true),
            ("systemd-on", false),
            ("systemd-unknown", false),
            ("launchd-off", false),
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::setup_finish_preserves_the_linux_logout_warning",
                    "--nocapture",
                ])
                .env(FIXTURE, case)
                .output()
                .unwrap();
            let text = String::from_utf8(output.stderr).unwrap();
            assert!(
                output.status.success(),
                "{case}: {text}\n{}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert_eq!(
                text.contains("stops when you log out"),
                warns,
                "{case}: {text}"
            );
            assert_eq!(
                text.contains("sudo loginctl enable-linger \"$USER\""),
                warns,
                "{case}: {text}"
            );
        }
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
    let served = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = koidrive_ipc::read_line(&mut stream).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<Request>(&request).unwrap(),
            Request::Status
        ));
        let status = Status {
            running: true,
            folders: vec![
                serde_json::from_value(serde_json::json!({
                    "space_id": "s1",
                    "space_name": "Koidrive",
                    "path": "/fixture/Koidrive",
                    "state": FolderState::UpToDate,
                    "history_id": null,
                    "last_seq": null,
                    "head_seq": null,
                    "remote_nodes": 0,
                    "local_entries": null,
                    "questions": 0,
                    "problem": null,
                    "wait": null
                }))
                .unwrap(),
            ],
            ..Status::default()
        };
        koidrive_ipc::write_line(&mut stream, &Response::Status { status })
            .await
            .unwrap();
    });
    let (manager, lingers) = match case.as_str() {
        "systemd-off" => ("systemd", Some(false)),
        "systemd-on" => ("systemd", Some(true)),
        "systemd-unknown" => ("systemd", None),
        "launchd-off" => ("launchd", Some(false)),
        _ => panic!("unknown fixture {case}"),
    };
    let report: service::Report = serde_json::from_value(serde_json::json!({
        "manager":manager, "name":"fixture", "state":"running", "lingers":lingers
    }))
    .unwrap();
    let note = "Setup complete. Add files to ~/Koidrive.\nCheck sync: koi status\n";
    let done = finish_setup(
        &paths,
        Done {
            note: note.into(),
            ..Done::printed()
        },
        Some(report),
        None,
        &terminal::Terminal::setup(true, true),
    )
    .await
    .unwrap();
    served.await.unwrap();
    assert_eq!(done.note, note);
    assert_eq!(done.data["service"]["manager"], manager);
}

#[test]
fn every_command_parses_in_its_group() {
    let parses = [
        &["koi", "status", "--check"][..],
        &["koi", "resolve"],
        &["koi", "setup", "--interactive"],
        &["koi", "join", "K7QM-2XPD@files.example.com"],
        &["koi", "server", "deploy"],
        &["koi", "server", "add-device", "--worker", "koidrive"],
        &[
            "koi",
            "server",
            "claim",
            "files.example.com",
            "ABCD",
            "--user",
            "Ada",
        ],
        &["koi", "server", "upgrade", "--worker", "koidrive", "--yes"],
        &["koi", "server", "delete", "--worker", "koidrive", "--yes"],
        &[
            "koi",
            "server",
            "delete",
            "--bucket",
            "koidrive-1a2b",
            "--yes",
        ],
        &["koi", "space", "create", "Notes"],
        &["koi", "space", "list"],
        &["koi", "space", "usage"],
        &["koi", "folder", "link", "dir", "--space", "s"],
        &["koi", "folder", "unlink", "dir"],
        &["koi", "folder", "restart", "dir"],
        &["koi", "folder", "retry", "dir", "--resend"],
        &["koi", "folder", "retry", "dir", "--replan"],
        &["koi", "device", "invite"],
        &["koi", "device", "list"],
        &["koi", "device", "revoke", ID],
        &["koi", "device", "leave"],
        &["koi", "device", "leave", "--last-device"],
        &["koi", "history", "trash"],
        &["koi", "history", "versions", "a.txt"],
        &["koi", "history", "restore", "a.txt", "--node", "n1"],
        &["koi", "history", "requests"],
        &["koi", "_daemon", "--log-format", "json"],
    ];
    for args in parses {
        assert!(
            parse(args).is_ok(),
            "{args:?}: {}",
            parse(args).err().unwrap()
        );
    }
    assert!(matches!(
        parse(&["koi", "history", "trash"]),
        Ok(Some(Command::History {
            action: history::HistoryAction::Trash { folder }
        })) if folder == Path::new(".")
    ));
    assert!(matches!(
        parse(&["koi", "history", "requests"]),
        Ok(Some(Command::History {
            action: history::HistoryAction::Requests { folder }
        })) if folder == Path::new(".")
    ));
    assert!(parse(&["koi", "folder", "retry", "dir"]).is_err());
    assert!(parse(&["koi", "folder", "retry", "dir", "--resend", "--replan"]).is_err());
    assert!(parse(&["koi", "folder", "link", "--space", "s"]).is_err());
    assert!(parse(&["koi", "history", "versions"]).is_err());
}

#[test]
fn the_old_command_names_are_gone_without_aliases() {
    let old = [
        &["koi", "setup", "K7QM-2XPD@files.example.com"][..],
        &["koi", "link", "dir", "--space", "s"],
        &["koi", "unlink", "dir"],
        &["koi", "restart", "dir"],
        &["koi", "lift", "dir", "--resend"],
        &["koi", "folder", "lift", "dir", "--resend"],
        &["koi", "questions"],
        &["koi", "trash"],
        &["koi", "versions", "a.txt"],
        &["koi", "restore", "a.txt"],
        &["koi", "restores"],
        &["koi", "history", "restores"],
        &["koi", "usage"],
        &["koi", "daemon"],
        &["koi", "device", "add"],
        &["koi", "server", "uninstall"],
        &["koi", "server", "upgrade", "--server", "koidrive"],
        &["koi", "server", "add-device", "--server", "koidrive"],
        &["koi", "status", "--json=2"],
    ];
    for args in old {
        assert!(parse(args).is_err(), "{args:?} still parses");
    }
    let help = Cli::command().render_help().to_string();
    assert!(!help.contains("_daemon"), "{help}");
    for group in [
        "status", "resolve", "setup", "join", "server", "space", "folder", "device", "history",
    ] {
        assert!(
            help.contains(&format!("  {group} ")),
            "{group} missing from {help}"
        );
    }
    assert!(help.contains("Koidrive sync client"), "{help}");
}

#[test]
fn json_is_one_global_flag_for_schema_1() {
    for args in [
        &["koi", "--json", "status"][..],
        &["koi", "status", "--json"],
        &["koi", "status", "--json=1"],
        &["koi", "folder", "link", "dir", "--json", "--space", "s"],
        &["koi", "history", "versions", "a.txt", "--json"],
    ] {
        assert_eq!(Cli::try_parse_from(args).unwrap().json, Some(1), "{args:?}");
    }
    assert_eq!(Cli::try_parse_from(["koi", "status"]).unwrap().json, None);
    assert!(Cli::try_parse_from(["koi", "status", "--json=0"]).is_err());
}

#[test]
fn a_group_without_its_subcommand_shows_help_and_does_nothing() {
    for group in ["server", "space", "folder", "device", "history"] {
        let error = Cli::try_parse_from(["koi", group]).err().expect("help");
        assert_eq!(
            error.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
            "{group}"
        );
    }
    assert!(matches!(parse(&["koi"]), Ok(None)));
}

#[test]
fn the_command_path_and_json_are_read_from_the_raw_arguments() {
    let args = |words: &[&str]| -> Vec<OsString> { words.iter().map(OsString::from).collect() };
    assert_eq!(
        command_path(&args(&[
            "koi", "--json", "folder", "link", "dir", "--space", "s"
        ])),
        "folder link"
    );
    assert_eq!(command_path(&args(&["koi", "status", "--check"])), "status");
    assert_eq!(command_path(&args(&["koi", "history"])), "history");
    assert_eq!(command_path(&args(&["koi", "nonsense", "x"])), "");
    assert_eq!(command_path(&args(&["koi"])), "");
    assert_eq!(command_path(&args(&["koi", "-V", "space", "list"])), "");
    assert_eq!(
        command_path(&args(&["koi", "space", "--help", "list"])),
        "space"
    );
    assert_eq!(command_path(&args(&["koi", "--", "status"])), "");
    assert_eq!(
        command_path(&args(&["koi", "space", "--bogus", "help"])),
        "space"
    );
    assert_eq!(command_path(&args(&["koi", "-Vh", "space", "list"])), "");
    assert_eq!(command_path(&args(&["koi", "-hV", "folder", "link"])), "");
    assert_eq!(
        command_path(&args(&["koi", "space", "-V", "list"])),
        "space list"
    );
    assert!(json_requested(&args(&["koi", "linky", "--json"])));
    assert!(json_requested(&args(&["koi", "status", "--json=2"])));
    assert!(!json_requested(&args(&["koi", "status"])));
    assert!(!json_requested(&args(&[
        "koi", "history", "versions", "--", "--json"
    ])));
}

#[tokio::test]
async fn a_reload_refuses_a_credential_for_another_server() {
    use crate::setup::fake::{Fake, Reply};
    use prost::Message;

    let refused = || Reply {
        method: "POST",
        path: "/v1/token".into(),
        status: 401,
        content_type: "application/x-protobuf",
        body: koidrive_proto::v1::Error {
            code: koidrive_proto::v1::error::Code::Unauthenticated as i32,
            message: "unknown device or refresh credential".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    };
    let first_server = Fake::start(vec![refused(), refused(), refused()]).await;
    let other_server = Fake::start(Vec::new()).await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let first = Credential {
        server: first_server.base.to_string(),
        user_id: "same-user".into(),
        user_name: None,
        device_id: ID.into(),
        device_name: "same-device".into(),
        refresh_credential: "first-credential".into(),
    };
    first.save_new(&paths.credential()).unwrap();
    let reader = reading(&paths, &first).unwrap();
    let owner = Driver::open(paths.clone()).unwrap();
    owner.sign_out_device(ID).await.unwrap();
    let replacement = Credential {
        server: other_server.base.to_string(),
        refresh_credential: "other-server-credential".into(),
        ..first
    };
    owner.save_credential(replacement.clone()).await.unwrap();

    assert!(matches!(
        reader.access_token().await,
        Err(ApiError::Credential(_))
    ));
    let presented: Vec<String> = first_server
        .seen()
        .iter()
        .map(|seen| {
            koidrive_proto::v1::TokenRequest::decode(seen.body.as_slice())
                .unwrap()
                .refresh_credential
        })
        .collect();
    assert!(
        !presented.contains(&replacement.refresh_credential),
        "{presented:?}"
    );
}

#[tokio::test]
async fn a_reload_never_presents_another_profiles_credential_to_the_first_server() {
    use crate::setup::fake::{Fake, Reply};
    use prost::Message;

    let refused = || Reply {
        method: "POST",
        path: "/v1/token".into(),
        status: 401,
        content_type: "application/x-protobuf",
        body: koidrive_proto::v1::Error {
            code: koidrive_proto::v1::error::Code::Unauthenticated as i32,
            message: "unknown device or refresh credential".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    };
    let first_server = Fake::start(vec![refused(), refused(), refused()]).await;
    let other_server = Fake::start(Vec::new()).await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let first = Credential {
        server: first_server.base.to_string(),
        user_id: "first-user".into(),
        user_name: None,
        device_id: ID.into(),
        device_name: "first".into(),
        refresh_credential: "first-credential".into(),
    };
    first.save_new(&paths.credential()).unwrap();
    let reader = reading(&paths, &first).unwrap();
    let owner = Driver::open(paths.clone()).unwrap();
    owner.sign_out_device(ID).await.unwrap();
    let replacement = Credential {
        server: other_server.base.to_string(),
        user_id: "other-user".into(),
        user_name: None,
        device_id: "0190a0b0-0000-7000-8000-000000000002".into(),
        device_name: "other".into(),
        refresh_credential: "other-credential".into(),
    };
    owner.save_credential(replacement.clone()).await.unwrap();

    assert!(matches!(
        reader.access_token().await,
        Err(ApiError::Credential(_))
    ));
    let presented: Vec<String> = first_server
        .seen()
        .iter()
        .map(|seen| {
            koidrive_proto::v1::TokenRequest::decode(seen.body.as_slice())
                .unwrap()
                .refresh_credential
        })
        .collect();
    assert!(
        !presented.contains(&replacement.refresh_credential),
        "{presented:?}"
    );
}

fn held(
    (deletions, affected_files, space_files): (u64, u64, u64),
    exceeded: &[Limit],
    items: usize,
) -> Question {
    Question {
        id: "q1".into(),
        lineage: "q1".into(),
        space_id: "s1".into(),
        asked_at_ms: 1,
        recovery_cause: None,
        kind: QuestionKind::MassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded: exceeded.to_vec(),
        },
        items: (0..items)
            .map(|i| koidrive_driver::HeldItem {
                node_id: format!("n{i}"),
                path: format!("old/{i:02}"),
                files: 1,
                device_id: None,
                device_name: None,
                size: None,
                modified_at_ms: None,
            })
            .collect(),
    }
}

const ORIGINAL: &str = "0190a0b0-0000-7000-8000-00000000000a";
const COPY: &str = "0190a0b0-0000-7000-8000-00000000000b";
const DESK: &str = "0190a0b0-0000-7000-8000-0000000000d1";
const LAPTOP: &str = "0190a0b0-0000-7000-8000-0000000000d2";

fn conflict(copy_path: &str) -> Question {
    use koidrive_engine::question::{ConflictFile, Revisions};

    let file = |node_id: &str| ConflictFile {
        node_id: node_id.into(),
        revisions: Revisions {
            content_rev: 2,
            meta_rev: 1,
        },
    };
    let item = |node_id: &str, path: &str, device_id: &str, name: &str| koidrive_driver::HeldItem {
        node_id: node_id.into(),
        path: path.into(),
        files: 1,
        device_id: Some(device_id.into()),
        device_name: Some(name.into()),
        size: None,
        modified_at_ms: None,
    };
    Question {
        kind: QuestionKind::Conflict {
            original: file(ORIGINAL),
            copy: file(COPY),
            sent_in: None,
        },
        items: vec![
            item(COPY, copy_path, LAPTOP, "agent-box"),
            item(ORIGINAL, "docs/notes.txt", DESK, "Ada's Desktop"),
        ],
        ..held((0, 0, 0), &[], 0)
    }
}

#[test]
fn a_conflict_answer_keeps_the_node_it_names_or_both() {
    let question = conflict("docs/notes (conflict).txt");
    assert_eq!(
        answer_to(&question, &format!("keep-node:{ORIGINAL}")),
        Ok(Answer::KeepOriginal)
    );
    assert_eq!(
        answer_to(&question, &format!("keep-node:{COPY}")),
        Ok(Answer::KeepCopy)
    );
    assert_eq!(answer_to(&question, "keep-both"), Ok(Answer::KeepBoth));
    let expected = format!(
        "is not an answer to q1; use keep-node:{ORIGINAL} or keep-node:{COPY} or keep-both"
    );
    for word in [
        format!("keep-node:{DESK}"),
        "restore".to_owned(),
        "keep_both".to_owned(),
    ] {
        let error = answer_to(&question, &word).unwrap_err();
        assert_eq!(error, format!("{word} {expected}"));
    }
}

#[test]
fn a_decision_other_than_a_conflict_takes_the_answers_it_did() {
    let question = held((30, 30, 40), &[Limit::Share], 1);
    assert_eq!(answer_to(&question, "delete"), Ok(Answer::Delete));
    assert_eq!(answer_to(&question, "restore"), Ok(Answer::Restore));
    assert_eq!(
        answer_to(&question, "keep-both"),
        Err("keep-both is not an answer to q1; use delete or restore".to_owned())
    );
    assert_eq!(
        answer_to(&question, &format!("keep-node:{ORIGINAL}")),
        Err(format!(
            "keep-node:{ORIGINAL} is not an answer to q1; use delete or restore"
        ))
    );
}

#[test]
fn resolve_answers_a_conflict_only_with_its_own_words_and_names_them_otherwise() {
    let parse = |word: &str| match Cli::try_parse_from(["koi", "resolve", "q1", word]) {
        Ok(Cli {
            command:
                Some(Command::Resolve {
                    answer: Some(answer),
                    ..
                }),
            ..
        }) => answer,
        _ => panic!("resolve q1 {word} parses as an answer"),
    };
    let question = conflict("docs/notes (conflict).txt");
    assert_eq!(
        answer_to(&question, &parse(&format!("keep-node:{COPY}"))),
        Ok(Answer::KeepCopy)
    );
    assert_eq!(
        answer_to(&question, &parse("keep-both")),
        Ok(Answer::KeepBoth)
    );
    let valid = format!("use keep-node:{ORIGINAL} or keep-node:{COPY} or keep-both");
    for word in ["nonsense", "keep_original", "keep-node:", "delete"] {
        let error = answer_to(&question, &parse(word)).unwrap_err();
        assert_eq!(error, format!("{word} is not an answer to q1; {valid}"));
    }
}

#[test]
fn resolve_lists_both_files_of_a_conflict_with_the_names_of_their_writers_and_one_command_per_answer()
 {
    assert_eq!(
        question_text(&conflict("docs/notes (conflict).txt")),
        format!(
            "q1  space s1  docs/notes.txt has a conflict copy\n\
                 \x20   Original: docs/notes.txt, written by Ada's Desktop\n\
                 \x20   Conflict copy: 'docs/notes (conflict).txt', written by agent-box\n\
                 \x20 Keep docs/notes.txt and move 'docs/notes (conflict).txt' to the trash:\n\
                 \x20   koi resolve q1 keep-node:{ORIGINAL}\n\
                 \x20 Give docs/notes.txt the content of 'docs/notes (conflict).txt', keeping the old content in its \
                 version history, and move 'docs/notes (conflict).txt' to the trash:\n\
                 \x20   koi resolve q1 keep-node:{COPY}\n\
                 \x20 Keep both files:\n\
                 \x20   koi resolve q1 keep-both\n"
        )
    );
}

#[test]
fn status_names_a_newer_release_after_the_service_line_only_while_one_is_out() {
    use koidrive_driver::Update;

    let service = "Background service: running (koidrive.service)\n";
    let status = |update| Status {
        running: true,
        version: Some("0.1.0".into()),
        update,
        ..Status::default()
    };
    let available = status(Update::Available {
        latest: "0.1.2".into(),
        url: "https://github.com/koidrive/koidrive/releases/tag/v0.1.2".into(),
        checked_at_ms: 1,
    });
    let notice = |status: &Status, command| {
        UpdateNoticeData::of(
            &status.update,
            status
                .version
                .as_deref()
                .unwrap_or(env!("CARGO_PKG_VERSION")),
            command,
        )
    };
    let text = status_text(
        &available,
        &[],
        service,
        &notice(&available, Some("koi update")),
    );
    assert!(
        text.contains(&format!(
            "{service}Update: Koidrive 0.1.2 is out (this device runs 0.1.0). Run: koi update\n"
        )),
        "{text}"
    );
    for quiet in [
        Update::UpToDate { checked_at_ms: 1 },
        Update::Unknown,
        Update::Off,
    ] {
        let quiet = status(quiet);
        let text = status_text(&quiet, &[], service, &notice(&quiet, Some("koi update")));
        assert!(!text.contains("Update:"), "{text}");
    }
    let unversioned = Status {
        version: None,
        ..available.clone()
    };
    assert!(
        status_text(
            &unversioned,
            &[],
            service,
            &notice(&unversioned, Some("koi update"))
        )
        .contains(concat!(
            "(this device runs ",
            env!("CARGO_PKG_VERSION"),
            ")"
        ))
    );
    for command in ["brew upgrade --cask koidrive", "brew upgrade koidrive"] {
        let text = status_text(&available, &[], service, &notice(&available, Some(command)));
        assert!(text.contains(&format!("Run: {command}\n")), "{text}");
        assert!(!text.contains("koi update"), "{text}");
    }
    let text = status_text(&available, &[], service, &notice(&available, None));
    assert!(
        text.contains("Download: https://github.com/koidrive/koidrive/releases/tag/v0.1.2"),
        "{text}"
    );
    assert!(!text.contains("Run:"), "{text}");
}

#[test]
fn status_shows_a_conflict_under_its_folder_and_a_notification_names_its_file() {
    let question = conflict("docs/notes (conflict).txt");
    let brief = question_brief(&question, Some(LAPTOP));
    let lines: Vec<&str> = brief.lines().collect();
    assert_eq!(
        lines[..3],
        [
            "  q1: docs/notes.txt has a conflict copy",
            "    Original: docs/notes.txt, written by Ada's Desktop",
            "    Conflict copy: 'docs/notes (conflict).txt', written by agent-box (this device)",
        ]
    );
    assert_eq!(
        lines[3..]
            .iter()
            .filter(|line| line.starts_with("    koi resolve q1 "))
            .count(),
        3
    );
    assert_eq!(lines.len(), 9);
    assert_eq!(
        question_notice(&question),
        "docs/notes.txt has a conflict copy. Run koi resolve."
    );
}

#[test]
fn a_conflict_path_is_quoted_with_its_control_characters_escaped() {
    let question = conflict("docs/it's\nforged\u{1b}[2J");
    for text in [question_text(&question), question_brief(&question, None)] {
        assert_eq!(text.lines().count(), 9, "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");
        assert!(
            text.contains(r"Conflict copy: 'docs/it'\''s\u{a}forged\u{1b}[2J', written by"),
            "{text}"
        );
    }
    let mut unwritten = conflict("docs/notes (conflict).txt");
    unwritten.items[0].device_id = None;
    unwritten.items[0].device_name = None;
    assert!(
        question_text(&unwritten)
            .contains("Conflict copy: 'docs/notes (conflict).txt', written by an unknown device\n")
    );
}

#[test]
fn a_conflict_writer_the_device_list_does_not_name_is_shown_without_its_id() {
    let mut question = conflict("docs/notes (conflict).txt");
    for item in &mut question.items {
        item.device_name = None;
    }
    let lines = |text: String| -> Vec<String> {
        (text.lines())
            .filter(|line| line.contains("written by"))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(
        lines(question_brief(&question, Some(LAPTOP))),
        [
            "    Original: docs/notes.txt, written by another device",
            "    Conflict copy: 'docs/notes (conflict).txt', written by this device",
        ]
    );
    let detailed = question_text(&question);
    assert_eq!(
        lines(detailed.clone()),
        [
            "    Original: docs/notes.txt, written by a device",
            "    Conflict copy: 'docs/notes (conflict).txt', written by a device",
        ]
    );
    assert!(
        !detailed.contains(DESK) && !detailed.contains(LAPTOP),
        "{detailed}"
    );
}

#[test]
fn a_conflict_writer_name_is_shown_with_its_control_characters_escaped() {
    let mut question = conflict("docs/notes (conflict).txt");
    question.items[1].device_name = Some("Ada\u{1b}[2J\nforged".into());
    let text = question_text(&question);
    assert!(
        text.contains(r"Original: docs/notes.txt, written by Ada\u{1b}[2J\u{a}forged"),
        "{text}"
    );
    assert!(!text.contains('\u{1b}'), "{text}");
}

#[test]
fn a_conflict_decision_in_json_carries_each_writer_id_and_name() {
    let decision =
        serde_json::to_value(report::Decision::of(&conflict("docs/notes (conflict).txt"))).unwrap();
    let writers: Vec<(&Value, &Value, &Value)> = (decision["items"].as_array().unwrap().iter())
        .map(|item| (&item["node_id"], &item["device_id"], &item["device_name"]))
        .collect();
    assert_eq!(
        writers,
        [
            (
                &Value::from(COPY),
                &Value::from(LAPTOP),
                &Value::from("agent-box")
            ),
            (
                &Value::from(ORIGINAL),
                &Value::from(DESK),
                &Value::from("Ada's Desktop")
            ),
        ]
    );
}

#[test]
fn a_held_path_is_shown_with_its_control_characters_escaped() {
    let mut question = held((1, 15, 20), &[Limit::Share], 1);
    question.items[0].path = "old/a\nforged line\u{1b}[2J".into();
    for text in [question_text(&question), question_brief(&question, None)] {
        assert_eq!(
            text.lines().count(),
            6 - text.starts_with(' ') as usize,
            "{text}"
        );
        assert!(!text.contains('\u{1b}'), "{text}");
    }
}

#[test]
fn a_mass_delete_question_names_the_limit_it_crossed_and_lists_at_most_20_items() {
    let text = question_text(&held((1_001, 1_001, 5_000), &[Limit::Deletions], 25));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[..6],
        [
            "q1  space s1  held 1001 deletions that would remove 1001 of 5000 files, more than 1000 deletions in 10 minutes",
            "  Delete these files on the server and other devices:",
            "    koi resolve q1 delete",
            "  Download these files to this device again:",
            "    koi resolve q1 restore",
            "  old/00 (1 file)",
        ]
    );
    assert_eq!(lines.len(), 5 + ITEMS_SHOWN + 1);
    assert_eq!(lines[24], "  old/19 (1 file)");
    assert_eq!(lines[25], "  and 5 more; koi resolve --json lists them all");

    let text = question_text(&held((1, 15, 20), &[Limit::Share], 1));
    assert_eq!(
        text.lines().next(),
        Some(
            "q1  space s1  held 1 deletions that would remove 15 of 20 files, more than half of the space in 10 minutes"
        )
    );
    assert_eq!(text.lines().count(), 6);
}

#[test]
fn status_shows_a_decision_under_its_folder_with_its_first_path_and_one_full_command_per_answer() {
    assert_eq!(
        question_brief(&held((30, 30, 40), &[Limit::Share], 3), None),
        [
            "  q1: held 30 deletions that would remove 30 of 40 files, more than half of the space in 10 minutes; first path: old/00",
            "  Delete these files on the server and other devices:",
            "    koi resolve q1 delete",
            "  Download these files to this device again:",
            "    koi resolve q1 restore",
            "",
        ]
        .join("\n")
    );
    assert_eq!(
        question_brief(&held((30, 30, 40), &[Limit::Share], 0), None)
            .lines()
            .next(),
        Some(
            "  q1: held 30 deletions that would remove 30 of 40 files, more than half of the space in 10 minutes"
        )
    );
}

#[test]
fn json_output_and_the_commands_that_show_decisions_get_no_banner() {
    let shows = |args: &[&str]| Cli::try_parse_from(args).unwrap().shows_banner();
    assert!(shows(&["koi", "history", "trash", "."]));
    assert!(shows(&["koi", "device", "list"]));
    assert!(shows(&["koi", "folder", "link", "dir", "--space", "s"]));
    for args in [
        &["koi", "history", "trash", ".", "--json"][..],
        &["koi", "device", "list", "--json"],
        &["koi", "space", "list", "--json"],
        &["koi", "space", "usage", "--json"],
        &["koi", "status"],
        &["koi", "resolve"],
        &["koi", "resolve", "q1", "delete"],
        &["koi", "_daemon"],
    ] {
        assert!(!shows(args), "{args:?}");
    }
    let reports = |args: &[&str]| {
        (Cli::try_parse_from(args).unwrap().command)
            .is_some_and(|command| command.reports_pending())
    };
    assert!(reports(&["koi", "resolve", "q1", "delete", "--json"]));
    assert!(!reports(&["koi", "resolve", "--json"]));
    assert!(!reports(&["koi", "status", "--json"]));
}

#[test]
fn the_banner_counts_the_pending_decisions_and_names_resolve() {
    assert_eq!(banner(&[]), None);
    assert_eq!(
        banner(&[held((30, 30, 40), &[Limit::Share], 1)]).as_deref(),
        Some("1 needs you; run koi resolve")
    );
    assert_eq!(
        banner(&[
            held((30, 30, 40), &[Limit::Share], 1),
            held((2_000, 2_000, 9_000), &[Limit::Deletions], 1)
        ])
        .as_deref(),
        Some("2 need you; run koi resolve")
    );
}

#[test]
fn a_notification_names_the_decision_its_first_path_and_resolve() {
    assert_eq!(
        question_notice(&held((30, 30, 40), &[Limit::Share], 2)),
        "held 30 deletions that would remove 30 of 40 files, more than half of the space in 10 minutes; first path: old/00. Run koi resolve."
    );
}

#[test]
fn resolve_lists_with_no_arguments_and_answers_with_an_id_and_an_answer() {
    let parse = |args: &[&str]| Cli::try_parse_from(args).map(|cli| cli.command);
    assert!(matches!(
        parse(&["koi", "resolve"]),
        Ok(Some(Command::Resolve {
            id: None,
            answer: None,
        }))
    ));
    assert!(matches!(
        parse(&["koi", "resolve", "q1", "restore"]),
        Ok(Some(Command::Resolve {
            id: Some(_),
            answer: Some(answer),
            ..
        })) if answer == "restore"
    ));
    assert!(parse(&["koi", "resolve", "q1"]).is_err());
    assert_eq!(
        answer_to(&held((30, 30, 40), &[Limit::Share], 1), "later"),
        Err("later is not an answer to q1; use delete or restore".to_owned())
    );
    assert!(matches!(
        parse(&["koi", "resolve", "q1", "delete", "--json"]),
        Ok(Some(Command::Resolve { id: Some(_), .. }))
    ));
    assert!(parse(&["koi", "questions"]).is_err());
}

#[tokio::test]
async fn a_build_without_the_server_bundle_stops_before_the_cloudflare_login() {
    if option_env!("KOIDRIVE_SERVER_BUNDLE").is_some() {
        return;
    }
    for args in [
        vec!["koi", "server", "deploy"],
        vec!["koi", "server", "add-device"],
        vec!["koi", "server", "upgrade"],
        vec!["koi", "server", "delete"],
    ] {
        let Some(Command::Server { action }) = Cli::try_parse_from(&args).unwrap().command else {
            unreachable!("{args:?} is a server command");
        };
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            setup::server(
                &paths,
                action,
                false,
                false,
                Arc::new(terminal::Terminal::new(true, false)),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{args:?} waited for a login"));
        let error = outcome.unwrap_err();
        assert_eq!(error.code, Code::AuthRequired, "{args:?}: {error:?}");
    }
    let Err(missing) = setup::embedded_bundle() else {
        panic!("a build without the server bundle unpacked one");
    };
    assert!(
        missing.message.contains("built without a server bundle"),
        "{missing:?}"
    );
}

#[test]
fn a_question_names_every_limit_crossed_and_a_notice_takes_dismiss() {
    let both = [Limit::Deletions, Limit::Share];
    let text = question_text(&held((1_001, 3_000, 5_000), &both, 1));
    assert_eq!(
        text.lines().next(),
        Some(
            "q1  space s1  held 1001 deletions that would remove 3000 of 5000 files, more than 1000 deletions in 10 minutes and more than half of the space in 10 minutes"
        )
    );

    let notice = Question {
        kind: QuestionKind::RemoteMassDelete {
            deletions: 1,
            affected_files: 15,
            space_files: 20,
            exceeded: vec![Limit::Share],
        },
        ..held((1, 15, 20), &[Limit::Share], 0)
    };
    let text = question_text(&notice);
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [
            "q1  space s1  applied 1 deletions from other devices that removed 15 of 20 files, more than half of the space in 10 minutes",
            "  Close this notice:",
            "    koi resolve q1 dismiss",
        ]
    );
}

#[test]
fn a_first_link_question_reads_as_a_sentence_with_its_answers() {
    let mut question = held((0, 0, 0), &[], 1);
    question.kind = QuestionKind::Recovery {
        group: RecoveryGroup::CreatedOnThisDevice,
        sent_in: None,
        cause: None,
    };
    let text = question_text(&question);
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [
            "q1  space s1  files on this device that the space does not hold",
            "  Send this device's version to the server:",
            "    koi resolve q1 publish",
            "  Move this device's copies to the local trash and follow the server:",
            "    koi resolve q1 discard",
            "  old/00 (1 file)",
        ]
    );
}

#[test]
fn a_name_longer_than_255_bytes_is_refused_before_anything_is_sent() {
    let longest = "가".repeat(85);
    let too_long = format!("{longest}a");
    for args in [
        vec!["koi", "space", "create", &too_long],
        vec![
            "koi",
            "join",
            "K7QM-2XPD@files.example.com",
            "--name",
            &too_long,
        ],
        vec![
            "koi",
            "server",
            "claim",
            "files.example.com",
            "ABCD",
            "--user",
            &too_long,
        ],
    ] {
        let error = Cli::try_parse_from(&args)
            .err()
            .expect("a 256-byte name is refused");
        assert!(
            error
                .to_string()
                .contains("a name is at most 255 bytes of UTF-8, and this one has 256"),
            "{error}"
        );
    }
    assert!(Cli::try_parse_from(["koi", "space", "create", &longest]).is_ok());
}

#[test]
fn a_device_id_is_a_uuid_and_nothing_that_could_name_another_path() {
    assert_eq!(device_id(ID).as_deref(), Ok(ID));
    assert_eq!(device_id(&ID.to_uppercase()).as_deref(), Ok(ID));
    for text in [
        "",
        "../spaces",
        "0190a0b0-0000-7000-8000-00000000000",
        &format!("{ID}/x"),
        "0190a0b0x0000-7000-8000-000000000001",
    ] {
        assert!(device_id(text).is_err(), "{text:?}");
    }
}

#[test]
fn names_and_platforms_cannot_send_terminal_controls_or_forge_rows() {
    let forged = DeviceInfo {
        device_id: ID.into(),
        name: "\u{1b}[2J\u{1b}[Hsafe-looking\u{202e}".into(),
        platform: "linux\nFORGED ROW\u{9b}".into(),
        ..Default::default()
    };
    let table = device_table(&[forged], 0);
    assert_eq!(table.lines().count(), 2, "{table}");
    assert!(
        !table.chars().any(|c| c.is_control() && c != '\n'),
        "{table:?}"
    );
    assert!(
        table.contains(r"\u{1b}[2J\u{1b}[Hsafe-looking\u{202e}  linux\u{a}FORGED ROW\u{9b}"),
        "{table}"
    );
    assert_eq!(
        serde_json::to_value(DeviceData::of(&DeviceInfo {
            name: "a\nb".into(),
            ..Default::default()
        }))
        .unwrap()["name"],
        "a\nb"
    );
}

#[test]
fn times_read_as_how_long_ago() {
    let now = 1_000 * 86_400_000;
    assert_eq!(ago(0, now), "never");
    assert_eq!(ago(now - 59_000, now), "just now");
    assert_eq!(ago(now + 5_000, now), "just now");
    assert_eq!(ago(now - 5 * 60_000, now), "5 min ago");
    assert_eq!(ago(now - 47 * 3_600_000, now), "47 h ago");
    assert_eq!(ago(now - 3 * 86_400_000, now), "3 days ago");
}

#[test]
fn sizes_read_in_decimal_units() {
    assert_eq!(size(0), "0 B");
    assert_eq!(size(999), "999 B");
    assert_eq!(size(1_000), "1.0 kB");
    assert_eq!(size(999_949), "999.9 kB");
    assert_eq!(size(999_950), "1.0 MB");
    assert_eq!(size(1_500_000_000), "1.5 GB");
    assert_eq!(size(u64::MAX), "18446.7 PB");
}

#[test]
fn the_usage_report_lists_each_space_and_its_total() {
    let space = SpaceInfo {
        space_id: ID.into(),
        name: "Home\u{1b}[2J".into(),
        ..Default::default()
    };
    let usage = SpaceUsage {
        live_files: 1_204,
        live_bytes: 3_200_000_000,
        trashed_files: 1,
        trash_bytes: 40_100_000,
        old_versions: 30,
        old_version_bytes: 12_000_000,
        unreferenced_bytes: 1_500_000,
    };
    assert_eq!(
        usage_report(&[(space.clone(), usage)]),
        format!(
            "Home\\u{{1b}}[2J ({ID})\n\
                 \x20 Live files        1204 files     3.2 GB\n\
                 \x20 Trash                 1 file    40.1 MB\n\
                 \x20 Old versions     30 versions    12.0 MB\n\
                 \x20 Unreferenced                     1.5 MB\n\
                 \x20 Total                            3.3 GB\n\
                 \n\
                 Each stored chunk counts once, under the first of live files, trash and old versions that holds it.\n\
                 Unreferenced chunks are uploads not committed yet and chunks waiting out the grace period before deletion.\n"
        )
    );
    assert_eq!(usage_report(&[]), "No spaces.\n");
    let json = serde_json::to_value(UsageData::of(&space, &usage)).unwrap();
    assert_eq!(json["total_bytes"], 3_253_600_000u64);
    assert_eq!(json["trashed_files"], 1);
}

#[test]
fn the_space_list_shows_each_space_by_id_and_name() {
    let spaces = [SpaceInfo {
        space_id: ID.into(),
        name: "Notes\u{1b}[2J".into(),
        ..SpaceInfo::default()
    }];
    assert_eq!(
        space_table(&spaces),
        format!("ID                                    NAME\n{ID}  Notes\\u{{1b}}[2J\n")
    );
    assert_eq!(
        space_table(&[]),
        "No spaces. Create one with koi space create <name>.\n"
    );
}

#[test]
fn usage_output_shows_only_the_spaces_that_answered() {
    let answered = [(
        SpaceInfo {
            space_id: ID.into(),
            name: "Home".into(),
            ..Default::default()
        },
        SpaceUsage {
            live_files: 2,
            live_bytes: 300,
            ..Default::default()
        },
    )];
    let refused = || {
        vec![(
            SpaceInfo {
                space_id: "s2".into(),
                name: "Closed".into(),
                ..Default::default()
            },
            CliError::from(ApiError::Status { status: 503 }),
        )]
    };

    let empty = usage_done(&[], &[]).unwrap();
    assert_eq!(empty.text, "No spaces.\n");
    assert_eq!(
        empty.data["spaces"],
        serde_json::json!([]),
        "an account with no spaces"
    );

    let none = usage_done(&[], &refused()).unwrap_err();
    let none = none.partial.expect("the failure keeps what was read");
    assert_eq!(none.text, "", "every space refused");
    assert_eq!(
        none.data["spaces"],
        serde_json::json!([]),
        "every space refused"
    );

    let error = usage_done(&answered, &refused()).unwrap_err();
    assert_eq!(error.code, Code::Unavailable);
    assert!(
        error.message.starts_with("no usage for Closed (s2): "),
        "{error:?}"
    );
    let partial = error.partial.expect("the failure keeps what was read");
    let spaces = partial.data["spaces"].as_array().unwrap();
    assert_eq!(spaces.len(), 1, "{:#}", partial.data);
    assert_eq!(spaces[0]["space_id"], ID);
    assert_eq!(spaces[0]["live_bytes"], 300);
    assert_eq!(partial.data["refused"][0]["space_id"], "s2");
    assert!(partial.text.starts_with(&format!("Home ({ID})\n")));
    assert_eq!(
        partial.data["spaces"],
        usage_done(&answered, &[]).unwrap().data["spaces"],
        "a refused space adds nothing to the answered ones"
    );
}

#[tokio::test]
async fn a_refused_credential_stops_the_usage_and_keeps_what_was_read() {
    use crate::setup::fake::{Fake, proto};
    use koidrive_proto::v1::{AccessToken, error::Code as ProtoCode};
    let refusal = || {
        let mut reply = proto(
            "GET",
            "/v1/spaces/b/usage",
            koidrive_proto::v1::Error {
                code: ProtoCode::Unauthenticated as i32,
                ..Default::default()
            },
        );
        reply.status = 401;
        reply
    };
    let token = || {
        let token = AccessToken {
            token: "fixture-only".into(),
            expires_at_ms: i64::MAX,
            ..Default::default()
        };
        proto("POST", "/v1/token", token)
    };
    let read = SpaceUsage {
        live_bytes: 42,
        ..Default::default()
    };
    let fake = Fake::start(vec![
        token(),
        token(),
        proto("GET", "/v1/spaces/a/usage", read),
        refusal(),
        refusal(),
    ])
    .await;
    let auth = Auth::new(
        koidrive_net::Api::local_development(fake.base.clone()).unwrap(),
        koidrive_net::Credentials {
            user_id: "fixture-user".into(),
            device_id: "fixture-device".into(),
            refresh_credential: "fixture-only".into(),
        },
    );
    let spaces = ["a", "b", "c"]
        .into_iter()
        .map(|id| SpaceInfo {
            space_id: id.into(),
            name: id.into(),
            ..Default::default()
        })
        .collect();

    let error = usage(&auth, spaces).await.unwrap_err();

    assert_eq!(error.code, Code::AuthRequired, "{error:?}");
    let partial = error.partial.expect("the failure keeps what was read");
    let spaces = partial.data["spaces"].as_array().unwrap();
    assert_eq!(spaces.len(), 1, "{:#}", partial.data);
    assert_eq!(spaces[0]["space_id"], "a");
    assert_eq!(spaces[0]["live_bytes"], 42);
    assert_eq!(partial.data["refused"], serde_json::json!([]));
}

#[test]
fn the_table_marks_this_device_and_revoked_ones() {
    let now = 1_000 * 86_400_000;
    let device = |name: &str, current, last_seen_ms, revoked_at_ms| DeviceInfo {
        device_id: ID.into(),
        name: name.into(),
        platform: "linux".into(),
        kind: Kind::Device as i32,
        last_seen_ms,
        revoked_at_ms,
        current,
        ..Default::default()
    };
    let table = device_table(
        &[
            device("desk", true, now - 120_000, 0),
            device("lost laptop", false, 0, now - 2 * 86_400_000),
            device("phone", false, now - 7_200_000, 0),
        ],
        now,
    );
    let id = ID;
    assert_eq!(
        table,
        format!(
            "ID                                    NAME         PLATFORM  LAST SEEN  STATE\n\
                 {id}  desk         linux     2 min ago  this device\n\
                 {id}  lost laptop  linux     never      revoked 2 days ago\n\
                 {id}  phone        linux     2 h ago    active\n"
        )
    );
}

#[tokio::test]
async fn an_update_check_names_the_version_the_daemon_compared_the_release_with() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
    let served = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        koidrive_ipc::read_line(&mut stream).await.unwrap();
        let checked = Response::UpdateChecked {
            update: koidrive_driver::Update::Available {
                latest: "0.1.1".into(),
                url: "https://github.com/koidrive/koidrive/releases/tag/v0.1.1".into(),
                checked_at_ms: 1_763_000_000_000,
            },
            version: "0.1.0".into(),
        };
        koidrive_ipc::write_line(&mut stream, &checked)
            .await
            .unwrap();
    });

    let done = crate::update::check_updates(&paths).await.unwrap();
    served.await.unwrap();

    assert_eq!(done.data["update"]["version"], "0.1.0");
    assert_eq!(done.data["update"]["latest"], "0.1.1");
    assert_eq!(
        done.text,
        "Koidrive 0.1.1 is out (this device runs 0.1.0).\n"
    );
}
