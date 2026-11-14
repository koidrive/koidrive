// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use prost::Message;

struct Profile(tempfile::TempDir);

struct Fixture(Option<std::thread::JoinHandle<()>>);

impl Fixture {
    fn join(mut self) -> std::thread::Result<()> {
        self.0.take().unwrap().join()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take() {
            let _ = thread.join();
        }
    }
}

struct Capture {
    exit: i32,
    stdout: String,
    stderr: String,
    terminal: String,
}

#[test]
fn ipc_error_presentations_only_replace_terminal_messages() {
    for (args, tty, pipe_stdout, vars, width, concise) in [
        (vec!["status"], true, false, vec![], 100, true),
        (vec!["status", "--plain"], true, false, vec![], 100, true),
        (
            vec!["status"],
            true,
            false,
            vec![("TERM", "dumb")],
            100,
            true,
        ),
        (vec!["status"], true, false, vec![], 40, true),
        (vec!["status", "--verbose"], true, false, vec![], 100, false),
        (vec!["status"], false, false, vec![], 100, false),
        (vec!["status"], true, true, vec![], 100, false),
        (vec!["status", "--json"], false, false, vec![], 100, false),
        (vec!["status", "--json"], true, false, vec![], 100, false),
    ] {
        let profile = Profile::new();
        let server = profile.fixture(1, |request| {
            assert_eq!(request["request"], "status");
            json!({"response":"error", "message":"raw detail /private/state/credential.json parser fixture-only-secret",
                "human":"The selected folder cannot be read. Check its permissions.",
                "code":"usage", "next":[{"argv":["koi","status"]}]})
        });
        let mut vars = vars;
        vars.push(("NO_COLOR", "1"));
        let output = profile.run_at_width(&args, tty, pipe_stdout, &vars, width);
        server.join().unwrap();
        assert_eq!(output.exit, 2);
        let text = format!("{}{}{}", output.stdout, output.stderr, output.terminal);
        if concise {
            assert!(
                text.contains("The selected folder cannot be read. Check its permissions."),
                "{text}"
            );
            assert!(!text.contains("fixture-only-secret"), "{text}");
            assert!(!text.contains("credential.json"), "{text}");
        } else {
            assert!(
                text.contains(
                    "raw detail /private/state/credential.json parser fixture-only-secret"
                ),
                "{text}"
            );
            assert!(
                !text.contains("The selected folder cannot be read."),
                "{text}"
            );
        }
        if args.contains(&"--json") {
            let json: Value = serde_json::from_str(if tty {
                &output.terminal
            } else {
                &output.stdout
            })
            .unwrap();
            assert_eq!(
                json["error"]["message"],
                "raw detail /private/state/credential.json parser fixture-only-secret"
            );
            assert_eq!(json["error"]["code"], "usage");
            assert_eq!(json["next_commands"], json!([{"argv":["koi","status"]}]));
            assert!(json["error"].get("human").is_none());
        }
    }
}

#[test]
fn old_daemon_errors_and_unknown_causes_remain_readable() {
    for tty in [false, true] {
        let profile = Profile::new();
        let server = profile.fixture(
            1,
            |_| json!({"response":"error", "message":"A new specific failure"}),
        );
        let output = profile.run(&["status"], tty, false, &[("NO_COLOR", "1")]);
        server.join().unwrap();
        assert_eq!(output.exit, 1);
        assert_eq!(
            if tty { output.terminal } else { output.stderr },
            if tty {
                "■  A new specific failure\n"
            } else {
                "koi: A new specific failure\n"
            }
        );
    }
}

#[test]
fn stale_choices_keep_their_code_and_offer_current_choices() {
    for (tty, json_output) in [(true, false), (false, false), (false, true)] {
        let profile = Profile::new();
        let mut args = vec!["resolve", "fixture-choice", "restore"];
        if json_output {
            args.push("--json");
        }
        let output = profile.run(&args, tty, false, &[("NO_COLOR", "1")]);
        assert_eq!(output.exit, 7);
        let text = format!("{}{}{}", output.stdout, output.stderr, output.terminal);
        if tty {
            assert!(
                text.contains("That decision is no longer waiting for an answer"),
                "{text}"
            );
            assert!(text.contains("koi resolve"), "{text}");
            assert!(!text.contains("fixture-choice"), "{text}");
        } else if json_output {
            let json: Value = serde_json::from_str(&output.stdout).unwrap();
            assert_eq!(json["error"]["code"], "stale_decision");
            assert_eq!(
                json["error"]["message"],
                "no unanswered question has the id \"fixture-choice\"; koi resolve lists the decisions waiting now"
            );
            assert_eq!(
                json["next_commands"],
                json!([{"argv":["koi","resolve","--json"]}])
            );
        } else {
            assert_eq!(
                output.stderr,
                "koi: no unanswered question has the id \"fixture-choice\"; koi resolve lists the decisions waiting now\n"
            );
        }
    }
}

#[test]
fn unexpected_daemon_replies_do_not_dump_credentials_on_a_terminal() {
    for (tty, verbose, json_output) in [
        (true, false, false),
        (true, true, false),
        (false, false, false),
        (false, false, true),
    ] {
        let profile = Profile::new();
        let server = profile.fixture(1, |_| json!({"response":"shared", "link":{
            "id":"routine-id", "url":"https://example.invalid/share/fixture-only-bearer",
            "space_id":"space-internal", "space_name":"Work", "folder":null, "node_id":"node-internal",
            "target":{"path":"notes.txt", "kind":"file"}, "created_at_ms":1000,
            "expires_at_ms":null, "revoked_at_ms":null, "has_password":false, "state":"active"
        }}));
        let mut args = vec!["status"];
        if verbose {
            args.push("--verbose");
        }
        if json_output {
            args.push("--json");
        }
        let output = profile.run(&args, tty, false, &[("NO_COLOR", "1")]);
        server.join().unwrap();
        assert_eq!(output.exit, 1);
        let text = format!("{}{}{}", output.stdout, output.stderr, output.terminal);
        if tty && !verbose {
            assert!(
                text.contains("Background sync returned an unexpected response."),
                "{text}"
            );
            assert!(text.contains("result is unknown"), "{text}");
            assert!(text.contains("before repeating a command"), "{text}");
            assert!(!text.contains("fixture-only-bearer"), "{text}");
            assert!(!text.contains("space-internal"), "{text}");
        } else {
            assert!(text.contains("the daemon answered Shared"), "{text}");
            assert!(text.contains("routine-id"), "{text}");
            assert!(text.contains("Secret(..)"), "{text}");
        }
        assert!(!text.contains("fixture-only-bearer"), "{text}");
    }
}

#[test]
fn unreadable_ipc_replies_keep_parser_details_out_of_terminal_output() {
    for tty in [false, true] {
        let profile = Profile::new();
        let server = profile.fixture(
            1,
            |_| json!({"response":"future_response", "debug":"fixture-only-secret"}),
        );
        let output = profile.run(&["status"], tty, false, &[("NO_COLOR", "1")]);
        server.join().unwrap();
        assert_eq!(output.exit, 1);
        let text = format!("{}{}", output.stderr, output.terminal);
        if tty {
            assert!(
                text.contains("Background sync returned a response Koidrive could not read"),
                "{text}"
            );
            assert!(text.contains("result is unknown"), "{text}");
            assert!(!text.contains("future_response"), "{text}");
            assert!(!text.contains("daemon.sock"), "{text}");
        } else {
            assert!(text.contains("unknown variant `future_response`"), "{text}");
            assert!(text.contains("daemon.sock"), "{text}");
        }
        assert!(!text.contains("fixture-only-secret"), "{text}");
    }
}

#[test]
fn invalid_pairing_and_damaged_credentials_have_safe_terminal_actions() {
    for tty in [false, true] {
        let profile = Profile::new();
        let output = profile.run(&["join", "@bad/host"], tty, false, &[("NO_COLOR", "1")]);
        assert_eq!(output.exit, 2);
        let text = format!("{}{}", output.stderr, output.terminal);
        if tty {
            assert!(text.contains("Copy the full code"), "{text}");
            assert!(!text.contains("@bad/host"), "{text}");
        } else {
            assert!(text.contains("has no pairing code before the @"), "{text}");
        }
        let state = profile.0.path().join("state/koidrive");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let path = state.join("credential.json");
        fs::write(&path, "{fixture-only-broken}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let output = profile.run(&["space", "list"], tty, false, &[("NO_COLOR", "1")]);
        assert_eq!(output.exit, 1);
        let text = format!("{}{}", output.stderr, output.terminal);
        if tty {
            assert!(
                text.contains("saved sign-in is damaged or unreadable"),
                "{text}"
            );
            assert!(!text.contains("credential.json"), "{text}");
        } else {
            assert!(text.contains(&path.display().to_string()), "{text}");
        }
    }
}

#[test]
fn invalid_configuration_and_unlinked_folders_keep_their_raw_pipe_causes() {
    for tty in [false, true] {
        let profile = Profile::new();
        let state = profile.0.path().join("state/koidrive");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let config = profile.0.path().join("config/koidrive/config.toml");
        fs::write(&config, "unrecognized-key = true\n").unwrap();
        let output = profile.run(&["status"], tty, false, &[("NO_COLOR", "1")]);
        assert_eq!(output.exit, 2);
        let text = format!("{}{}", output.stderr, output.terminal);
        if tty {
            assert!(text.contains("configuration is not valid"), "{text}");
            assert!(
                !text.contains(&profile.0.path().display().to_string()),
                "{text}"
            );
            assert!(!text.contains("unrecognized-key"), "{text}");
        } else {
            assert!(text.contains("unknown field `unrecognized-key`"), "{text}");
            assert!(text.contains(&config.display().to_string()), "{text}");
        }
        fs::write(config, "[service]\nbackground = false\n").unwrap();
        let folder = profile.0.path().join("My notes");
        fs::create_dir(&folder).unwrap();
        let output = profile.run(
            &["folder", "restart", folder.to_str().unwrap()],
            tty,
            false,
            &[("NO_COLOR", "1")],
        );
        assert_eq!(output.exit, 2);
        let text = format!("{}{}", output.stderr, output.terminal);
        assert!(
            text.contains(if tty {
                "not linked to Koidrive"
            } else {
                "is not a linked folder"
            }),
            "{text}"
        );
    }
}

#[test]
fn missing_user_chosen_folders_keep_the_escaped_path_needed_to_correct_the_command() {
    for tty in [false, true] {
        let profile = Profile::new();
        profile.save_credential("http://127.0.0.1:9/");
        let folder = profile.0.path().join("Missing\nfolder");
        let output = profile.run(
            &[
                "folder",
                "link",
                folder.to_str().unwrap(),
                "--space",
                "Work",
            ],
            tty,
            false,
            &[("NO_COLOR", "1")],
        );
        assert_eq!(output.exit, 2);
        let text = format!("{}{}", output.stderr, output.terminal);
        assert!(
            text.contains(if tty {
                "Missing\\u{a}folder"
            } else {
                "Missing\\nfolder"
            }),
            "{text}"
        );
        assert!(!text.contains("Missing\nfolder"), "{text}");
        if tty {
            assert!(
                text.contains("Check its location and whether its drive is connected"),
                "{text}"
            );
            assert!(
                !text.contains(&profile.0.path().display().to_string()),
                "{text}"
            );
        } else {
            assert!(
                text.contains(&profile.0.path().display().to_string()),
                "{text}"
            );
        }
    }
}

#[test]
fn foreground_diagnostics_require_verbose_or_an_explicit_log_filter_on_terminals() {
    for (tty, extra, filter, diagnostic) in [
        (true, None, None, false),
        (true, Some("--plain"), None, false),
        (true, Some("--verbose"), None, true),
        (true, None, Some("warn"), true),
        (false, None, None, true),
        (true, Some("--json"), None, true),
    ] {
        let profile = Profile::new();
        let state = profile.0.path().join("state/koidrive");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("telemetry"), "fixture blocks settings directory").unwrap();
        let mut args = vec!["setup", "--telemetry", "off", "--no-app"];
        if let Some(extra) = extra {
            args.push(extra);
        }
        let mut vars = vec![("NO_COLOR", "1")];
        if let Some(filter) = filter {
            vars.push(("KOIDRIVE_LOG", filter));
        }
        let output = profile.run(&args, tty, false, &vars);
        let text = format!("{}{}{}", output.stdout, output.stderr, output.terminal);
        assert_eq!(text.contains("koi::telemetry"), diagnostic, "{text}");
        assert!(
            text.contains("the telemetry choice could not be stored"),
            "{text}"
        );
        assert_eq!(text.contains("error="), diagnostic, "{text}");
    }
}

#[test]
fn local_api_fixtures_keep_status_causes_and_unknown_outcome_guidance() {
    for (status, body, raw, concise, exit, code) in [
        (
            503,
            Vec::new(),
            "the server answered HTTP 503 without an error message",
            "The server answered HTTP 503 without a readable cause.",
            6,
            "unavailable",
        ),
        (
            200,
            vec![255],
            "the server sent a body that is not a valid AccessToken",
            "Koidrive could not read the server's answer.",
            1,
            "failed",
        ),
        (
            403,
            koidrive_proto::v1::Error {
                code: koidrive_proto::v1::error::Code::Forbidden as i32,
                message: "The owner disabled this operation".into(),
                ..Default::default()
            }
            .encode_to_vec(),
            "the server answered HTTP 403: CODE_FORBIDDEN: The owner disabled this operation",
            "The server refused the request. The owner disabled this operation",
            1,
            "failed",
        ),
    ] {
        for (tty, verbose, json_output) in [
            (true, false, false),
            (true, true, false),
            (false, false, false),
            (false, false, true),
        ] {
            let profile = Profile::new();
            let server = profile.http(status, body.clone());
            let mut args = vec!["space", "list"];
            if verbose {
                args.push("--verbose");
            }
            if json_output {
                args.push("--json");
            }
            let output = profile.run(&args, tty, false, &[("NO_COLOR", "1"), ("NO_PROXY", "*")]);
            server.join().unwrap();
            assert_eq!(output.exit, exit);
            let text = format!("{}{}{}", output.stdout, output.stderr, output.terminal);
            if tty && !verbose {
                assert!(text.contains(concise), "{text}");
                assert!(!text.contains(raw), "{text}");
                if status != 403 {
                    assert!(text.contains("Before repeating a change"), "{text}");
                }
            } else if json_output {
                let json: Value = serde_json::from_str(&output.stdout).unwrap();
                assert_eq!(json["error"]["message"], raw);
                assert_eq!(json["error"]["code"], code);
                assert_eq!(json["next_commands"], json!([]));
            } else {
                assert!(text.contains(raw), "{text}");
                assert!(!text.contains(concise), "{text}");
            }
            assert!(!text.contains("fixture-only-refresh"), "{text}");
        }
    }
}

#[test]
fn a_closed_local_server_has_a_connection_action_without_a_blind_mutation_retry() {
    for tty in [false, true] {
        let profile = Profile::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        profile.save_credential(&server);
        let output = profile.run(
            &["space", "list"],
            tty,
            false,
            &[("NO_COLOR", "1"), ("NO_PROXY", "*")],
        );
        assert_eq!(output.exit, 6);
        let text = format!("{}{}", output.stderr, output.terminal);
        if tty {
            assert!(
                text.contains("Koidrive could not reach the server. Check your connection."),
                "{text}"
            );
            assert!(text.contains("Before repeating a change"), "{text}");
            assert!(!text.contains("fixture-only-refresh"), "{text}");
        } else {
            assert_eq!(output.stderr, "koi: could not reach the server\n");
        }
    }
}

#[test]
fn history_rpc_failures_keep_specific_causes_without_protocol_details() {
    use koidrive_proto::v1::{RpcResponse, TreePage, VersionList, rpc_response};

    let reply = |result| RpcResponse { req_id: 1, result }.encode_to_vec();
    let repeated = || {
        reply(Some(rpc_response::Result::Tree(TreePage {
            next_cursor: "fixture-page-marker".into(),
            ..Default::default()
        })))
    };
    for (pages, raw, human) in [
        (
            vec![reply(None)],
            "the server sent an empty answer",
            "The server returned no history result.",
        ),
        (
            vec![reply(Some(rpc_response::Result::Versions(
                VersionList::default(),
            )))],
            "the server answered with another kind of result",
            "The server returned a different history result than requested.",
        ),
        (
            vec![repeated(), repeated()],
            "the server sent the same page cursor twice",
            "The server repeated its history page marker",
        ),
        (
            vec![reply(Some(rpc_response::Result::Error(
                koidrive_proto::v1::Error {
                    code: koidrive_proto::v1::error::Code::Forbidden as i32,
                    message: "History access was removed".into(),
                    ..Default::default()
                },
            )))],
            "the server refused: History access was removed",
            "The server refused the request. History access was removed",
        ),
    ] {
        for (tty, json_output) in [(true, false), (false, false), (false, true)] {
            let profile = Profile::new();
            let folder = profile.0.path().join("Work");
            fs::create_dir(&folder).unwrap();
            let path = fs::canonicalize(&folder).unwrap().display().to_string();
            let mut replies = vec![(
                "POST /v1/token ",
                200,
                koidrive_proto::v1::AccessToken {
                    token: "fixture-only-token".into(),
                    expires_at_ms: 4_102_444_800_000,
                    ..Default::default()
                }
                .encode_to_vec(),
            )];
            replies.extend(
                pages
                    .iter()
                    .cloned()
                    .map(|page| ("POST /v1/spaces/s1/rpc ", 200, page)),
            );
            let http = profile.http_responses(replies);
            let daemon = profile.fixture(2, move |request| match request["request"].as_str().unwrap() {
                "status" => json!({"response":"status", "status":{"account":null,"running":true,"folders":[{
                    "space_id":"s1","space_name":"Work","path":path,"state":"stopped",
                    "history_id":null,"last_seq":null,"head_seq":null,"remote_nodes":0,
                    "local_entries":0,"questions":0,"problem":null
                }]}}),
                "questions" => json!({"response":"questions","questions":[]}),
                other => panic!("unexpected request {other}"),
            });
            let mut args = vec!["history", "trash", folder.to_str().unwrap()];
            if json_output {
                args.push("--json");
            }
            let output = profile.run(&args, tty, false, &[("NO_COLOR", "1"), ("NO_PROXY", "*")]);
            http.join().unwrap();
            daemon.join().unwrap();
            assert_eq!(output.exit, 1);
            let text = format!("{}{}{}", output.stdout, output.stderr, output.terminal);
            if tty {
                assert!(text.contains(human), "{text}");
                assert!(!text.contains(raw), "{text}");
            } else if json_output {
                let json: Value = serde_json::from_str(&output.stdout).unwrap();
                assert_eq!(json["error"]["message"], raw);
                assert_eq!(json["error"]["code"], "failed");
            } else {
                assert_eq!(output.stderr, format!("koi: {raw}\n"));
            }
            assert!(!text.contains("fixture-page-marker"), "{text}");
            assert!(!text.contains("fixture-only-token"), "{text}");
        }
    }
}

#[test]
fn last_device_sign_out_keeps_the_unconfirmed_revocation_warning() {
    for (tty, extra, structured) in [
        (true, None, false),
        (true, Some("--plain"), false),
        (true, Some("--verbose"), true),
        (false, None, true),
        (false, Some("--json"), true),
    ] {
        let profile = Profile::new();
        let server = profile.http_responses(vec![
            (
                "POST /v1/token ",
                200,
                koidrive_proto::v1::AccessToken {
                    token: "fixture-only-token".into(),
                    expires_at_ms: 4_102_444_800_000,
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
            (
                "DELETE /v1/devices/device-internal?confirm=last-device ",
                503,
                Vec::new(),
            ),
        ]);
        let bin = profile.0.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for name in ["launchctl", "systemctl", "loginctl", "open"] {
            let path = bin.join(name);
            fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let mut args = vec!["device", "leave", "--last-device"];
        if let Some(extra) = extra {
            args.push(extra);
        }
        let output = profile.run(
            &args,
            tty,
            false,
            &[("PATH", &path), ("NO_COLOR", "1"), ("NO_PROXY", "*")],
        );
        server.join().unwrap();
        assert_eq!(output.exit, 0);
        assert!(
            !profile
                .0
                .path()
                .join("state/koidrive/credential.json")
                .exists()
        );
        let text = format!("{}{}{}", output.stdout, output.stderr, output.terminal);
        assert!(
            text.contains("the server did not revoke this device; revoke it from another device"),
            "{text}"
        );
        assert_eq!(
            text.contains("koidrive_driver::driver"),
            structured,
            "{text}"
        );
        assert_eq!(text.contains("failure="), structured, "{text}");
        if !structured {
            assert!(!text.contains("HTTP 503"), "{text}");
            assert!(!text.contains("fixture-only-refresh"), "{text}");
        }
    }
}

#[test]
fn last_device_refusal_keeps_owner_recovery_and_the_confirmation_command() {
    for (tty, verbose, json_output) in [
        (true, false, false),
        (true, true, false),
        (false, false, false),
        (false, false, true),
    ] {
        let profile = Profile::new();
        let server = profile.http_responses(vec![
            (
                "POST /v1/token ",
                200,
                koidrive_proto::v1::AccessToken {
                    token: "fixture-only-token".into(),
                    expires_at_ms: 4_102_444_800_000,
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
            ("DELETE /v1/devices/device-internal ", 409, Vec::new()),
        ]);
        let mut args = vec!["device", "leave"];
        if verbose {
            args.push("--verbose");
        }
        if json_output {
            args.push("--json");
        }
        let output = profile.run(&args, tty, false, &[("NO_COLOR", "1"), ("NO_PROXY", "*")]);
        server.join().unwrap();
        assert_eq!(output.exit, 1);
        assert!(
            profile
                .0
                .path()
                .join("state/koidrive/credential.json")
                .exists()
        );
        let text = format!("{}{}{}", output.stdout, output.stderr, output.terminal);
        assert!(text.contains("last active device"), "{text}");
        assert!(text.contains("owner recovery"), "{text}");
        let raw = "this is the last active device of its user; after it signs out, only owner recovery adds a device again";
        if tty && !verbose {
            assert!(
                text.contains("koi device leave --last-device only if you accept that risk"),
                "{text}"
            );
        } else if json_output {
            let json: Value = serde_json::from_str(&output.stdout).unwrap();
            assert_eq!(json["error"]["message"], raw);
            assert_eq!(json["error"]["code"], "failed");
            assert_eq!(
                json["next_commands"],
                json!([{"argv":["koi","device","leave","--last-device"]}])
            );
        } else {
            assert!(text.contains(raw), "{text}");
        }
    }
}

impl Profile {
    fn new() -> Self {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        fs::create_dir_all(home.path().join("config/koidrive")).unwrap();
        fs::write(
            home.path().join("config/koidrive/config.toml"),
            "[service]\nbackground = false\n",
        )
        .unwrap();
        Self(home)
    }

    fn http(&self, status: u16, body: Vec<u8>) -> Fixture {
        self.http_responses(vec![("POST /v1/token ", status, body)])
    }

    fn http_responses(&self, replies: Vec<(&'static str, u16, Vec<u8>)>) -> Fixture {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = Fixture(Some(std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            for (expected, status, body) in replies {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing HTTP fixture request");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                assert!(first.starts_with(expected), "{first}");
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(
                        reader.read_line(&mut line).unwrap() > 0,
                        "HTTP fixture request ended before its headers"
                    );
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                write!(
                stream,
                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/x-protobuf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
                stream.write_all(&body).unwrap();
            }
        })));
        self.save_credential(&format!("http://{address}/"));
        server
    }

    fn save_credential(&self, server: &str) {
        let state = self.0.path().join("state/koidrive");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let credential = state.join("credential.json");
        fs::write(&credential, json!({"server":server,"user_id":"owner","device_id":"device-internal","device_name":"Laptop","refresh_credential":"fixture-only-refresh"}).to_string()).unwrap();
        fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn run(&self, args: &[&str], tty: bool, pipe_stdout: bool, vars: &[(&str, &str)]) -> Capture {
        self.run_at_width(args, tty, pipe_stdout, vars, 100)
    }

    fn run_at_width(
        &self,
        args: &[&str],
        tty: bool,
        pipe_stdout: bool,
        vars: &[(&str, &str)],
        width: u16,
    ) -> Capture {
        let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
        command
            .args(args)
            .current_dir(self.0.path())
            .env("HOME", self.0.path())
            .env("XDG_CONFIG_HOME", self.0.path().join("config"))
            .env("XDG_STATE_HOME", self.0.path().join("state"))
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("KOI_PLAIN")
            .env_remove("KOIDRIVE_LOG")
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR")
            .env_remove("CLICOLOR_FORCE")
            .env("TERM", "xterm-256color")
            .env("CI", "1")
            .env("DO_NOT_TRACK", "1")
            .env("KOIDRIVE_TELEMETRY", "0")
            .env("KOIDRIVE_UPDATE_CHECK", "0")
            .envs(vars.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let reader = if tty {
            let master = rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR).unwrap();
            rustix::pty::grantpt(&master).unwrap();
            rustix::pty::unlockpt(&master).unwrap();
            let name = rustix::pty::ptsname(&master, Vec::new()).unwrap();
            let slave = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(name.to_str().unwrap())
                .unwrap();
            rustix::termios::tcsetwinsize(
                &slave,
                rustix::termios::Winsize {
                    ws_row: 32,
                    ws_col: width,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                },
            )
            .unwrap();
            command.stderr(slave.try_clone().unwrap());
            if !pipe_stdout {
                command.stdout(slave);
            }
            Some(File::from(master))
        } else {
            None
        };
        let mut child = command.spawn().unwrap();
        drop(command);
        let reader = reader.map(|mut master| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    match master.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                        Err(error)
                            if error.raw_os_error()
                                == Some(rustix::io::Errno::IO.raw_os_error()) =>
                        {
                            break;
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => panic!("{error}"),
                    }
                }
                String::from_utf8(bytes).unwrap().replace("\r\n", "\n")
            })
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut timed_out = false;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        let terminal = reader
            .map(|reader| reader.join().unwrap())
            .unwrap_or_default();
        assert!(!timed_out, "koi {args:?} did not exit");
        Capture {
            exit: output.status.code().unwrap(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
            terminal,
        }
    }

    fn fixture(&self, count: usize, reply: impl Fn(Value) -> Value + Send + 'static) -> Fixture {
        let state = self.0.path().join("state/koidrive");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let listener = UnixListener::bind(state.join("daemon.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        Fixture(Some(std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            for _ in 0..count {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing fixture request");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let response = reply(request);
                writeln!(stream, "{response}").unwrap();
            }
        })))
    }
}

#[test]
fn terminal_results_use_the_setup_theme() {
    let output = Profile::new().run(&["resolve"], true, false, &[("NO_COLOR", "1")]);
    assert_eq!(output.exit, 0);
    assert_eq!(output.terminal, "●  No decisions are waiting.\n");
}

#[test]
fn summary_regression_space_link_hints_are_copyable_and_terminal_safe() {
    use prost::Message;

    for name in ["Notes", "Team's notes $(exit 99)", "Work\u{1b}[2J\u{202e}"] {
        let profile = Profile::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let server_url = format!("http://{}/", listener.local_addr().unwrap());
        let credential = profile.0.path().join("state/koidrive/credential.json");
        fs::create_dir_all(credential.parent().unwrap()).unwrap();
        fs::write(
            &credential,
            serde_json::to_vec(&json!({
                "server":server_url, "user_id":"owner", "device_id":"local-device",
                "device_name":"Laptop", "refresh_credential":"fixture-only"
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            let replies = [
                koidrive_proto::v1::AccessToken {
                    token: "fixture-only".into(),
                    expires_at_ms: i64::MAX,
                    ..Default::default()
                }
                .encode_to_vec(),
                koidrive_proto::v1::SpaceList {
                    spaces: vec![koidrive_proto::v1::SpaceInfo {
                        space_id: "space-fixture".into(),
                        name: name.into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }
                .encode_to_vec(),
            ];
            for body in replies {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing fixture request");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                loop {
                    line.clear();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        let output = profile.run(
            &["space", "create", name],
            true,
            false,
            &[("NO_COLOR", "1")],
        );
        server.join().unwrap();
        assert_eq!(output.exit, 0, "{}", output.terminal);
        assert!(!output.terminal.contains('\u{1b}'), "{:?}", output.terminal);
        assert!(
            !output.terminal.contains('\u{202e}'),
            "{:?}",
            output.terminal
        );
        let command = output
            .terminal
            .lines()
            .find_map(|line| {
                line.split_once("koi folder link <folder> --space ")
                    .map(|(_, word)| word)
            })
            .unwrap();
        let parsed = Command::new("/bin/sh")
            .args(["-c", &format!("printf '%s' {command}")])
            .output()
            .unwrap();
        assert!(parsed.status.success());
        assert_eq!(
            String::from_utf8(parsed.stdout).unwrap(),
            "space-fixture",
            "{}",
            output.terminal
        );
    }
}

#[test]
fn summary_regression_standalone_decisions_keep_their_scope() {
    let ids = [
        "0190f0a0-0000-7000-8000-000000000001",
        "0190f0a0-0000-7000-8000-000000000002",
    ];
    for context in ["missing", "linked", "unreadable"] {
        let profile = Profile::new();
        let state = profile.0.path().join("state/koidrive");
        fs::create_dir_all(&state).unwrap();
        if context == "linked" {
            for (id, folder) in ids.iter().zip(["Work Files", "Personal"]) {
                let store = state.join("spaces").join(id).join("store.sqlite");
                fs::create_dir_all(store.parent().unwrap()).unwrap();
                koidrive_store::Store::open(&store)
                    .unwrap()
                    .transaction(|tx| {
                        tx.set_link(&koidrive_store::Link {
                            space_id: (*id).into(),
                            space_name: folder.into(),
                            root: profile.0.path().join(folder),
                            root_dev: 1,
                            server_url: "https://files.example.com/".into(),
                            device_id: "local".into(),
                            linked_at_ms: 1,
                        })
                    })
                    .unwrap();
            }
        } else if context == "unreadable" {
            fs::write(state.join("spaces"), "not a directory").unwrap();
        }
        let _held = (context == "linked").then(|| {
            let lock = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(state.join("lock"))
                .unwrap();
            lock.lock().unwrap();
            lock
        });
        let folders: Vec<_> = ids
            .iter()
            .zip(["Work Files", "Personal"])
            .map(|(id, folder)| {
                json!({
                    "space_id":id, "space_name":folder, "path":profile.0.path().join(folder),
                    "state":"up_to_date", "history_id":null, "last_seq":null, "head_seq":null,
                    "remote_nodes":0, "local_entries":0, "questions":1, "problem":null
                })
            })
            .collect();
        let server = profile.fixture(if context == "linked" { 2 } else { 1 }, move |request| {
            match request["request"].as_str().unwrap() {
            "questions" => json!({"response":"questions", "questions":ids.iter().enumerate().map(|(index, id)| json!({
                "id":format!("q{index}"), "space_id":id, "asked_at_ms":1000,
                "kind":{"type":"ambiguous_disappearance"},
                "items":[{"node_id":"n1", "path":"report.docx", "files":1}]
            })).collect::<Vec<_>>() }),
            "status" => json!({"response":"status", "status":{"account":null, "running":true, "folders":folders}}),
            other => panic!("unexpected fixture request {other}"),
            }
        });
        let output = profile.run(&["resolve"], true, false, &[("NO_COLOR", "1")]);
        server.join().unwrap();
        assert_eq!(output.exit, 0);
        if context == "linked" {
            for (index, folder) in ["~/Work Files", "~/Personal"].iter().enumerate() {
                assert!(
                    output
                        .terminal
                        .contains(&format!("q{index}  folder {folder}  files disappeared")),
                    "{}",
                    output.terminal
                );
            }
            for id in ids {
                assert!(!output.terminal.contains(id), "{}", output.terminal);
            }
        } else {
            for (index, id) in ids.iter().enumerate() {
                assert!(
                    output
                        .terminal
                        .contains(&format!("q{index}  space {id}  files disappeared")),
                    "{}",
                    output.terminal
                );
            }
            assert!(
                output.terminal.contains("koi status --verbose"),
                "{}",
                output.terminal
            );
        }
    }
}

#[test]
fn summary_regression_offline_causes_show_active_and_scheduled_retries() {
    for active in [true, false] {
        let profile = Profile::new();
        let path = profile.0.path().join("Work").display().to_string();
        let server = profile.fixture(1, move |request| {
            assert_eq!(request["request"], "status");
            json!({"response":"status", "status":{
                "account":null, "running":true,
                "folders":[{"space_id":"s1", "space_name":"Work", "path":path,
                    "state":"offline", "history_id":null, "last_seq":null, "head_seq":null,
                    "remote_nodes":0, "local_entries":0, "questions":0, "problem":null,
                    "wait":{"kind":"retry", "until_ms":if active { Value::Null } else { json!(i64::MAX) },
                        "unreachable":{"why":"the server's address could not be found", "no_internet":false}}}]
            }})
        });
        let output = profile.run(&["status"], true, false, &[("NO_COLOR", "1")]);
        server.join().unwrap();
        assert_eq!(output.exit, 0);
        assert!(
            output.terminal.contains("~/Work: Offline."),
            "{}",
            output.terminal
        );
        assert!(
            output
                .terminal
                .contains("Cannot reach the server: the server's address could not be found."),
            "{}",
            output.terminal
        );
        assert!(
            !output.terminal.contains("Offline. Check your connection"),
            "{}",
            output.terminal
        );
        assert!(
            output.terminal.contains(if active {
                "Retrying now."
            } else {
                "Retry in about"
            }),
            "{}",
            output.terminal
        );
    }
}

#[test]
fn summary_regression_pending_restores_explain_wait_causes() {
    for (kind, cause) in [
        ("quota", "Storage is full or a daily limit was reached."),
        ("retry", "Sync is waiting to retry an operation."),
    ] {
        let profile = Profile::new();
        let folder = profile.0.path().join("Work");
        fs::create_dir(&folder).unwrap();
        let path = fs::canonicalize(folder).unwrap().display().to_string();
        let server = profile.fixture(3, move |request| match request["request"].as_str().unwrap() {
            "status" => json!({"response":"status", "status":{
                "account":null, "running":true, "folders":[{"space_id":"s1", "space_name":"Work", "path":path,
                    "state":"up_to_date","history_id":null,"last_seq":null,"head_seq":null,
                    "remote_nodes":0,"local_entries":0,"questions":0,"problem":null,
                    "wait":{"kind":kind,"until_ms":0}}]
            }}),
            "restores" => json!({"response":"restores", "restores":[{
                "id":"restore-internal", "path":"report.docx", "asked_at_ms":1000,
                "request":{"type":"from_trash", "node_id":"node-internal", "parent_id":null, "name":null},
                "state":{"type":"pending", "waits_for":null}
            }]}),
            "questions" => json!({"response":"questions", "questions":[]}),
            other => panic!("unexpected fixture request {other}"),
        });
        let output = profile.run(
            &["history", "requests", "Work"],
            true,
            false,
            &[("NO_COLOR", "1")],
        );
        server.join().unwrap();
        assert_eq!(output.exit, 0);
        assert!(output.terminal.starts_with("▲  "), "{}", output.terminal);
        assert!(output.terminal.contains(cause), "{}", output.terminal);
        assert!(
            output.terminal.contains("Retry is due now."),
            "{}",
            output.terminal
        );
    }
}

#[test]
fn terminal_errors_use_the_setup_error_symbol() {
    let output = Profile::new().run(&["space", "list"], true, false, &[("NO_COLOR", "1")]);
    assert_eq!(output.exit, 4);
    assert_eq!(
        output.terminal,
        "■  This device is not set up. Run koi setup to connect it.\n"
    );
}

#[test]
fn argument_errors_share_the_terminal_renderer_without_changing_pipes() {
    let profile = Profile::new();
    let pipe = profile.run(&["doctor"], false, false, &[("NO_COLOR", "1")]);
    let tty = profile.run(&["doctor"], true, false, &[("NO_COLOR", "1")]);
    assert_eq!(pipe.exit, 2);
    assert_eq!(tty.exit, 2);
    assert!(
        pipe.stderr
            .starts_with("error: unrecognized subcommand 'doctor'\n")
    );
    assert!(
        tty.terminal
            .starts_with("■  unrecognized subcommand 'doctor'\n")
    );
    assert!(tty.terminal.contains("│  Usage: koi [OPTIONS] [COMMAND]\n"));
}

#[test]
fn a_color_terminal_uses_the_setup_colors() {
    let output = Profile::new().run(&["resolve"], true, false, &[("CLICOLOR_FORCE", "1")]);
    assert!(output.terminal.contains('\u{1b}'));
    assert!(output.terminal.contains("No decisions are waiting."));

    let profile = Profile::new();
    let state = profile.0.path().join("state/koidrive");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("telemetry"), "fixture blocks settings directory").unwrap();
    let output = profile.run(
        &["setup", "--telemetry", "off", "--no-app"],
        true,
        false,
        &[("CLICOLOR_FORCE", "1")],
    );
    let warning = output
        .terminal
        .lines()
        .find(|line| line.contains("the telemetry choice could not be stored"))
        .unwrap();
    assert!(warning.contains('\u{1b}'), "{warning}");
    assert!(warning.contains('▲'), "{warning}");
    assert!(!warning.contains("error="), "{warning}");
}

#[test]
fn pipes_keep_the_original_result_and_error_bytes() {
    let profile = Profile::new();
    let output = profile.run(&["resolve"], false, false, &[("CLICOLOR_FORCE", "1")]);
    assert_eq!(output.stdout, "No decisions are waiting.\n");
    assert!(output.stderr.is_empty());
    let output = profile.run(&["space", "list"], false, false, &[("CLICOLOR_FORCE", "1")]);
    assert_eq!(output.exit, 4);
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        "koi: this device is not set up; run koi setup first\n"
    );
}

#[test]
fn plain_switches_and_dumb_terminals_keep_line_output() {
    for (args, vars) in [
        (vec!["resolve", "--plain"], vec![]),
        (vec!["resolve"], vec![("KOI_PLAIN", "1")]),
        (vec!["resolve"], vec![("TERM", "dumb")]),
    ] {
        let output = Profile::new().run(&args, true, false, &vars);
        assert_eq!(output.terminal, "No decisions are waiting.\n");
    }
}

#[test]
fn redirecting_stdout_keeps_stderr_plain_too() {
    let output = Profile::new().run(&["space", "list"], true, true, &[]);
    assert_eq!(
        output.terminal,
        "koi: this device is not set up; run koi setup first\n"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn terminal_json_has_no_presentation_characters() {
    let output = Profile::new().run(
        &["resolve", "--json"],
        true,
        false,
        &[("CLICOLOR_FORCE", "1")],
    );
    let envelope: Value = serde_json::from_str(&output.terminal).unwrap();
    assert_eq!(envelope["outcome"], "ok");
    assert_eq!(envelope["decisions"], json!([]));
    assert!(envelope["data"].is_null());
    assert!(!output.terminal.contains('\u{1b}'));
}

#[test]
fn help_is_verbatim_on_a_terminal() {
    let profile = Profile::new();
    let pipe = profile.run(&[], false, false, &[("NO_COLOR", "1")]);
    let tty = profile.run(&[], true, false, &[("NO_COLOR", "1")]);
    assert_eq!(tty.terminal, pipe.stdout);
}

#[test]
fn bearer_values_stay_bare_and_explanations_are_styled() {
    for tty in [false, true] {
        let profile = Profile::new();
        let server = profile.fixture(2, |request| match request["request"].as_str().unwrap() {
            "share" => json!({"response":"shared", "link":{
                "id":"0123456789", "url":"https://files.example.com/s/AAAAAAAAAAAAAAAAAAAAAA",
                "space_id":"s1", "space_name":"Team", "folder":null, "node_id":"n1",
                "target":{"path":"notes.txt", "kind":"file"}, "created_at_ms":1000,
                "expires_at_ms":null, "revoked_at_ms":null, "has_password":false, "state":"active"
            }}),
            "questions" => json!({"response":"questions", "questions":[]}),
            other => panic!("unexpected fixture request {other}"),
        });
        let output = profile.run(
            &["share", "create", "notes.txt"],
            tty,
            false,
            &[("NO_COLOR", "1")],
        );
        server.join().unwrap();
        assert_eq!(output.exit, 0);
        let url = "https://files.example.com/s/AAAAAAAAAAAAAAAAAAAAAA\n";
        if tty {
            assert!(output.terminal.starts_with(url), "{}", output.terminal);
            assert!(
                output
                    .terminal
                    .contains("│  To end it: koi share revoke 0123456789\n")
            );
        } else {
            assert_eq!(output.stdout, url);
            assert!(
                output
                    .stderr
                    .starts_with("Anyone with this link can download notes.txt")
            );
            assert!(!output.stderr.contains('│'));
        }
    }
}

#[test]
fn status_reports_setup_and_halted_folders_as_needing_attention() {
    let output = Profile::new().run(&["status"], true, false, &[("NO_COLOR", "1")]);
    assert_eq!(output.exit, 0);
    assert!(
        output
            .terminal
            .starts_with("▲  This device is not set up. Run koi setup.\n")
    );
    let profile = Profile::new();
    let path = profile.0.path().join("Work").display().to_string();
    let server = profile.fixture(1, move |request| {
        assert_eq!(request["request"], "status");
        json!({"response":"status", "status":{
            "account":{"server":"https://files.example.com/", "user_id":"owner",
                "device_id":"d1", "device_name":"Laptop"},
            "running":true, "folders":[{"space_id":"s1", "space_name":"Work", "path":path,
                "state":"halted","history_id":null,"last_seq":null,"head_seq":null,
                "remote_nodes":0,"local_entries":0,"questions":0,"problem":"fixture disk error"}]
        }})
    });
    let output = profile.run(&["status"], true, false, &[("NO_COLOR", "1")]);
    server.join().unwrap();
    assert_eq!(output.exit, 0);
    assert!(
        output.terminal.starts_with("▲  Connected as Laptop"),
        "{}",
        output.terminal
    );
    assert!(output.terminal.contains("Sync stopped"));
    assert!(output.terminal.contains("fixture disk error"));
    assert!(!output.terminal.contains('◇'));
}

#[test]
fn narrow_terminals_use_the_same_plain_fallback_as_setup() {
    for width in [40, 59] {
        let output = Profile::new().run_at_width(
            &["resolve"],
            true,
            false,
            &[("CLICOLOR_FORCE", "1")],
            width,
        );
        assert_eq!(output.terminal, "No decisions are waiting.\n");
        let output = Profile::new().run_at_width(
            &["space", "list"],
            true,
            false,
            &[("CLICOLOR_FORCE", "1")],
            width,
        );
        assert_eq!(
            output.terminal,
            "koi: This device is not set up. Run koi setup to connect it.\n"
        );
    }
    let output = Profile::new().run_at_width(&["resolve"], true, false, &[("NO_COLOR", "1")], 60);
    assert_eq!(output.terminal, "●  No decisions are waiting.\n");
}

#[test]
fn pending_decisions_keep_the_plain_prefix_out_of_rich_output() {
    for tty in [false, true] {
        let profile = Profile::new();
        let server = profile.fixture(2, |request| match request["request"].as_str().unwrap() {
            "pause" => json!({"response":"already_paused"}),
            "questions" => json!({"response":"questions", "questions":[{
                "id":"q1", "space_id":"s1", "asked_at_ms":1000,
                "kind":{"type":"ambiguous_disappearance"}, "items":[]
            }]}),
            other => panic!("unexpected fixture request {other}"),
        });
        let output = profile.run(&["pause"], tty, false, &[("NO_COLOR", "1")]);
        server.join().unwrap();
        assert_eq!(output.exit, 0);
        if tty {
            assert!(
                output
                    .terminal
                    .contains("▲  1 needs you; run koi resolve\n")
            );
            assert!(!output.terminal.contains("koi: "));
        } else {
            assert_eq!(output.stdout, "Sync is already paused.\n");
            assert_eq!(output.stderr, "koi: 1 needs you; run koi resolve\n");
        }
    }
}

#[test]
fn status_summarizes_every_state_and_verbose_keeps_engine_details() {
    for (state, shown) in [
        ("stopped", "Not syncing"),
        ("connecting", "Connecting"),
        ("bootstrapping", "Preparing the first sync"),
        ("catching_up", "Syncing"),
        ("up_to_date", "Up to date"),
        ("offline", "Offline. Check your connection"),
        ("halted", "Sync stopped"),
    ] {
        for (tty, verbose) in [(true, false), (true, true), (false, false)] {
            let profile = Profile::new();
            let path = profile.0.path().join("Work").display().to_string();
            let server = profile.fixture(1, move |request| {
                assert_eq!(request["request"], "status");
                json!({"response":"status", "status":{
                    "account":{"server":"https://files.example.com/", "user_id":"owner",
                        "device_id":"device-internal", "device_name":"Laptop"},
                    "running":true, "paused":false,
                    "folders":[{"space_id":"space-internal", "space_name":"Work", "path":path,
                        "state":state,"history_id":null,"last_seq":42,"head_seq":50,
                        "remote_nodes":99,"local_entries":88,"questions":0,"problem":null}]
                }})
            });
            let args = if verbose {
                vec!["status", "--verbose"]
            } else {
                vec!["status"]
            };
            let output = profile.run(&args, tty, false, &[("NO_COLOR", "1")]);
            server.join().unwrap();
            assert_eq!(output.exit, 0);
            let text = if tty {
                &output.terminal
            } else {
                &output.stdout
            };
            if tty && !verbose {
                assert!(text.contains(&format!("~/Work: {shown}.")), "{text}");
                for hidden in [
                    "device-internal",
                    "space-internal",
                    "seq 42",
                    "99 remote nodes",
                    "88 local entries",
                ] {
                    assert!(!text.contains(hidden), "{text}");
                }
                assert!(
                    !text.contains(&profile.0.path().display().to_string()),
                    "{text}"
                );
            } else {
                for detail in [
                    "device-internal",
                    "space-internal",
                    "seq 42 of 50",
                    "99 remote nodes",
                    "88 local entries",
                ] {
                    assert!(text.contains(detail), "{text}");
                }
            }
        }
    }
}

#[test]
fn concise_resolve_uses_the_local_device_for_conflict_choices() {
    let profile = Profile::new();
    let server = profile.fixture(1, |request| {
        assert_eq!(request["request"], "questions");
        json!({"response":"questions", "questions":[{
            "id":"q1", "space_id":"s1", "asked_at_ms":1000,
            "kind":{"type":"conflict",
                "original":{"node_id":"n1","revisions":{"content_rev":2,"meta_rev":1}},
                "copy":{"node_id":"n2","revisions":{"content_rev":1,"meta_rev":1}}},
            "items":[
                {"node_id":"n1","path":"deck.key","files":1,"device_id":"local-device"},
                {"node_id":"n2","path":"deck (conflict).key","files":1,"device_id":"other-device"}
            ]
        }]})
    });
    let credential = profile.0.path().join("state/koidrive/credential.json");
    fs::write(
        &credential,
        serde_json::to_vec(&json!({
            "server":"https://files.example.com/", "user_id":"owner", "device_id":"local-device",
            "device_name":"Laptop", "refresh_credential":"fixture-only"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
    let output = profile.run(&["resolve"], true, false, &[("NO_COLOR", "1")]);
    server.join().unwrap();
    assert_eq!(output.exit, 0);
    for expected in [
        "written by this device",
        "written by another device",
        "keep-node:n1",
        "keep-node:n2",
    ] {
        assert!(output.terminal.contains(expected), "{}", output.terminal);
    }
    assert!(!output.terminal.contains("local-device"));
    assert!(!output.terminal.contains("other-device"));
}

#[test]
fn concise_restore_notes_keep_attention_on_stopped_folders() {
    use sha2::{Digest, Sha256};

    for state in ["stopped", "halted", "offline"] {
        let profile = Profile::new();
        let root = fs::canonicalize(profile.0.path()).unwrap();
        let folder = root.join("Work");
        fs::create_dir(&folder).unwrap();
        let path = folder.display().to_string();
        let server = profile.fixture(2, move |request| match request["request"].as_str().unwrap() {
            "status" => json!({"response":"status", "status":{
                "account":null, "running":true, "folders":[{"space_id":"s1", "space_name":"Work", "path":path,
                    "state":state,"history_id":null,"last_seq":null,"head_seq":null,
                    "remote_nodes":0,"local_entries":0,"questions":0,"problem":null}]
            }}),
            "questions" => json!({"response":"questions", "questions":[]}),
            other => panic!("unexpected fixture request {other}"),
        });
        let records = root.join("state/koidrive/requests");
        fs::create_dir(&records).unwrap();
        let digest: String = Sha256::digest(b"restore-fixture")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let name = format!("{digest}.json");
        let key = koidrive_ipc::Key::restore(&folder.join("report.docx"), None, None);
        fs::write(
            records.join(name),
            serde_json::to_vec(&json!({
                "id":"restore-fixture", "key":key, "outcome":{
                    "response":"restore", "id":"restore-1", "space_id":"s1",
                    "folder":folder, "path":"report.docx"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let output = profile.run(
            &[
                "history",
                "restore",
                "Work/report.docx",
                "--request-id",
                "restore-fixture",
            ],
            true,
            false,
            &[("NO_COLOR", "1")],
        );
        server.join().unwrap();
        assert_eq!(output.exit, 0);
        assert!(
            output
                .terminal
                .starts_with("restore-1\n▲  Restore requested"),
            "{}",
            output.terminal
        );
        assert!(
            output.terminal.contains("The folder is not syncing."),
            "{}",
            output.terminal
        );
    }
}

#[test]
fn a_saved_choice_does_not_claim_that_sync_finished() {
    for (tty, verbose) in [(true, false), (true, true), (false, false)] {
        let profile = Profile::new();
        let server = profile.fixture(2, |request| match request["request"].as_str().unwrap() {
            "questions" => json!({"response":"questions", "questions":[{
                "id":"q1", "space_id":"s1", "asked_at_ms":1000,
                "kind":{"type":"ambiguous_disappearance"}, "items":[]
            }]}),
            "answer" => json!({"response":"answered"}),
            other => panic!("unexpected fixture request {other}"),
        });
        let args = if verbose {
            vec!["resolve", "q1", "restore", "--verbose"]
        } else {
            vec!["resolve", "q1", "restore"]
        };
        let output = profile.run(&args, tty, false, &[("NO_COLOR", "1")]);
        server.join().unwrap();
        assert_eq!(output.exit, 0);
        if tty && !verbose {
            assert_eq!(output.terminal, "●  Choice saved. Sync will apply it.\n");
        } else if tty {
            assert_eq!(output.terminal, "●  Resolved.\n");
        } else {
            assert_eq!(output.stdout, "Resolved.\n");
        }
    }
}

#[test]
fn telemetry_keeps_install_ids_in_verbose_and_pipe_output_only() {
    let profile = Profile::new();
    assert_eq!(profile.run(&["telemetry", "on"], false, false, &[]).exit, 0);
    let summary = profile.run(&["telemetry", "status"], true, false, &[("NO_COLOR", "1")]);
    assert!(summary.terminal.contains("DO_NOT_TRACK is set"));
    assert!(!summary.terminal.contains("Install id:"));
    for (tty, args) in [
        (true, vec!["telemetry", "status", "--verbose"]),
        (false, vec!["telemetry", "status"]),
    ] {
        let output = profile.run(&args, tty, false, &[("NO_COLOR", "1")]);
        let text = if tty { output.terminal } else { output.stdout };
        assert!(text.contains("Install id:"), "{text}");
    }
    let output = profile.run(&["telemetry", "reset"], true, false, &[("NO_COLOR", "1")]);
    assert!(
        output
            .terminal
            .contains("Usage report ID reset. Unsent reports were deleted.")
    );
    assert!(!output.terminal.contains("New install id:"));
}

#[test]
fn restore_requests_show_paths_and_supported_progress_guidance() {
    for (tty, verbose) in [(true, false), (true, true), (false, false)] {
        let profile = Profile::new();
        let folder = profile.0.path().join("Work");
        fs::create_dir(&folder).unwrap();
        let path = fs::canonicalize(folder).unwrap().display().to_string();
        let server = profile.fixture(3, move |request| match request["request"].as_str().unwrap() {
            "status" => json!({"response":"status", "status":{
                "account":null, "running":true, "folders":[{"space_id":"s1", "space_name":"Work", "path":path,
                    "state":"stopped","history_id":null,"last_seq":null,"head_seq":null,
                    "remote_nodes":0,"local_entries":0,"questions":0,"problem":null}]
            }}),
            "restores" => json!({"response":"restores", "restores":[{
                "id":"restore-internal", "path":"report.docx", "asked_at_ms":1000,
                "request":{"type":"from_trash", "node_id":"node-internal", "parent_id":null, "name":null},
                "state":{"type":"pending", "waits_for":null}
            }]}),
            "questions" => json!({"response":"questions", "questions":[]}),
            other => panic!("unexpected fixture request {other}"),
        });
        let args = if verbose {
            vec!["history", "requests", "Work", "--verbose"]
        } else {
            vec!["history", "requests", "Work"]
        };
        let output = profile.run(&args, tty, false, &[("NO_COLOR", "1")]);
        server.join().unwrap();
        assert_eq!(output.exit, 0);
        let text = if tty { output.terminal } else { output.stdout };
        if tty && !verbose {
            assert!(
                text.contains("report.docx: pending: waits for the folder to sync"),
                "{text}"
            );
            assert!(text.starts_with("▲  "), "{text}");
            assert!(text.contains("koi status"));
            assert!(!text.contains("node-internal"));
            assert!(!text.contains("koi _daemon"));
        } else {
            assert!(
                text.contains("restore-internal  deleted node node-internal"),
                "{text}"
            );
            assert!(text.contains("koi _daemon"));
        }
    }
}

#[test]
fn concise_status_does_not_require_color_or_a_wide_terminal() {
    for (width, args, vars) in [
        (40, vec!["status"], vec![]),
        (59, vec!["status"], vec![]),
        (100, vec!["status", "--plain"], vec![]),
        (100, vec!["status"], vec![("KOI_PLAIN", "1")]),
        (100, vec!["status"], vec![("TERM", "dumb")]),
    ] {
        let profile = Profile::new();
        let path = profile.0.path().join("Work").display().to_string();
        let server = profile.fixture(1, move |request| {
            assert_eq!(request["request"], "status");
            json!({"response":"status", "status":{
                "account":{"server":"https://files.example.com/", "user_id":"owner",
                    "device_id":"device-internal", "device_name":"Laptop"},
                "running":true, "folders":[{"space_id":"space-internal", "space_name":"Work", "path":path,
                    "state":"up_to_date","history_id":null,"last_seq":42,"head_seq":50,
                    "remote_nodes":99,"local_entries":88,"questions":0,"problem":null}]
            }})
        });
        let output = profile.run_at_width(&args, true, false, &vars, width);
        server.join().unwrap();
        assert_eq!(output.exit, 0);
        for expected in ["Connected as Laptop.", "~/Work: Up to date."] {
            assert!(output.terminal.contains(expected), "{}", output.terminal);
        }
        for hidden in [
            "device-internal",
            "space-internal",
            "seq 42",
            "remote nodes",
            "│",
            "\u{1b}",
        ] {
            assert!(!output.terminal.contains(hidden), "{}", output.terminal);
        }
    }
}
