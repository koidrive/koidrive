// SPDX-License-Identifier: Apache-2.0

use std::process::{Command, Output, Stdio};

use serde_json::Value;

mod schema;

struct Profile {
    home: tempfile::TempDir,
}

impl Profile {
    fn new() -> Self {
        Profile {
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let home = self.home.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
        command
            .args(args)
            .current_dir(home)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_RUNTIME_DIR", home.join("run"))
            .env_remove("KOIDRIVE_LOG")
            .stdin(Stdio::null());
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run koi")
    }

    fn json(&self, args: &[&str]) -> (i32, Value) {
        let output = self.output(args);
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?} printed no single JSON object ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        schema::check(&envelope);
        for field in [
            "command",
            "outcome",
            "data",
            "error",
            "decisions",
            "next_commands",
        ] {
            assert!(
                envelope.get(field).is_some(),
                "{field} missing: {envelope:#}"
            );
        }
        (output.status.code().unwrap(), envelope)
    }
}

fn argv(next: &Value) -> Vec<&str> {
    next["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn pair_and_claim_read_codes_from_stdin_or_positional_arguments() {
    use koidrive_ipc::{Code, Request, Response};
    use tokio::io::AsyncWriteExt;

    for (prefix, code, expected) in [
        (
            vec![
                "join",
                "--server",
                "https://files.example.com",
                "--name",
                "Mac",
            ],
            "K7QM-2XPD",
            Request::Pair {
                server: "https://files.example.com".into(),
                code: "K7QM-2XPD".into(),
                name: "Mac".into(),
            },
        ),
        (
            vec![
                "server",
                "claim",
                "files.example.com",
                "--user",
                "Ada",
                "--name",
                "Mac",
            ],
            "ABCD-EFGH",
            Request::Claim {
                server: "https://files.example.com".into(),
                code: "ABCD-EFGH".into(),
                user_name: "Ada".into(),
                name: "Mac".into(),
            },
        ),
    ] {
        for stdin in [true, false] {
            let profile = Profile::new();
            let home = profile.home.path();
            let paths = koidrive_driver::Paths::from_vars(|name| match name {
                "XDG_CONFIG_HOME" => Some(home.join("config").into()),
                "XDG_STATE_HOME" => Some(home.join("state").into()),
                "XDG_RUNTIME_DIR" => Some(home.join("run").into()),
                _ => None,
            })
            .unwrap();
            let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
            let expected = expected.clone();
            let mut served = tokio::spawn(async move {
                for expected in [expected, Request::Questions] {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let line = koidrive_ipc::read_line(&mut stream).await.unwrap();
                    assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), expected);
                    let response = match expected {
                        Request::Questions => Response::Questions {
                            questions: Vec::new(),
                        },
                        _ => Response::Error {
                            code: Code::WrongCode,
                            message: "the code is wrong or has expired".into(),
                            reason: None,
                            next: Vec::new(),
                        },
                    };
                    koidrive_ipc::write_line(&mut stream, &response)
                        .await
                        .unwrap();
                }
            });
            let mut args = prefix.clone();
            args.extend([
                if stdin { "--code-stdin" } else { code },
                "--json",
                "--telemetry",
                "off",
            ]);
            let mut child = tokio::process::Command::from(profile.command(&args))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            if stdin {
                input
                    .write_all(format!(" {code} \r\n").as_bytes())
                    .await
                    .unwrap();
            }
            drop(input);
            let output =
                tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
                    .await;
            if output.is_err() {
                served.abort();
                let _ = served.await;
                panic!("the child did not finish after reading the code");
            }
            let output = output.unwrap().unwrap();
            match tokio::time::timeout(std::time::Duration::from_secs(2), &mut served).await {
                Ok(result) => result.unwrap(),
                Err(_) => {
                    served.abort();
                    let _ = served.await;
                    panic!("the child exited before sending both requests: {output:?}");
                }
            }
            assert_eq!(output.status.code(), Some(8), "{output:?}");
            let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
            schema::check(&envelope);
            assert_eq!(envelope["error"]["code"], "wrong_code");
        }
    }
}

#[test]
fn app_policy_skips_installation_without_changing_the_service_report() {
    let profile = Profile::new();
    let config = profile.home.path().join("config/koidrive");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        "[service]\nbackground = false\n",
    )
    .unwrap();
    for (args, reason) in [
        (vec!["setup", "--no-app", "--json"], "opt_out"),
        (vec!["setup", "--json"], "unattended_setup"),
        (vec!["_reconcile", "--json"], "ci"),
        (vec!["_reconcile", "--no-app", "--json"], "service_handoff"),
    ] {
        let output = profile.command(&args).env("CI", "1").output().unwrap();
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(output.status.success(), "{envelope:#}");
        schema::check(&envelope);
        assert_eq!(
            envelope["data"]["app"],
            serde_json::json!({"state":"skipped", "reason":reason})
        );
        assert_eq!(envelope["data"]["service"]["state"], "off");
        assert!(
            !profile
                .home
                .path()
                .join("Library/Caches/dev.koidrive.app-install")
                .exists()
        );
        assert!(!profile.home.path().join("Applications").exists());
    }
}

#[test]
fn interrupt_during_telemetry_upload_prints_one_json_envelope() {
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let profile = Profile::new();
    let mut child = profile
        .command(&["join", "not-a-code", "--telemetry", "on", "--json"])
        .env_remove("DO_NOT_TRACK")
        .env_remove("KOIDRIVE_TELEMETRY")
        .env("KOIDRIVE_TELEMETRY_TOKEN", "test-project-token")
        .env(
            "KOIDRIVE_TELEMETRY_URL",
            format!("http://{}", listener.local_addr().unwrap()),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let (_upload, _) = loop {
        if let Ok(connection) = listener.accept() {
            break connection;
        }
        if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "no telemetry upload: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    rustix::process::kill_process(
        rustix::process::Pid::from_child(&child),
        rustix::process::Signal::INT,
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "not one JSON envelope ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    schema::check(&envelope);
    assert_eq!(envelope["error"]["code"], "interrupted");
}

#[test]
fn status_reads_a_profile_that_is_not_set_up_and_check_exits_4() {
    let profile = Profile::new();
    let (code, envelope) = profile.json(&["status", "--json"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(envelope["command"], "status");
    assert_eq!(envelope["outcome"], "ok");
    assert_eq!(envelope["error"], Value::Null);
    assert_eq!(envelope["data"]["account"], Value::Null);
    assert_eq!(envelope["data"]["folders"], serde_json::json!([]));
    assert_eq!(
        envelope["data"]["conditions"],
        serde_json::json!([{ "condition": "not_set_up" }])
    );
    assert_eq!(argv(&envelope["next_commands"][0]), ["koi", "setup"]);

    let (code, _) = profile.json(&["--json", "status", "--check"]);
    assert_eq!(code, 4);
    assert_eq!(
        profile.output(&["status", "--check"]).status.code(),
        Some(4)
    );
    assert_eq!(profile.output(&["status"]).status.code(), Some(0));
}

#[test]
fn errors_are_envelopes_with_their_code_and_exit_status() {
    let profile = Profile::new();

    let (code, envelope) = profile.json(&["resolve", "q1", "delete", "--json"]);
    assert_eq!(code, 7, "{envelope:#}");
    assert_eq!(envelope["outcome"], "error");
    assert_eq!(envelope["error"]["code"], "stale_decision");
    assert_eq!(envelope["error"]["retryable"], false);
    assert_eq!(
        argv(&envelope["next_commands"][0]),
        ["koi", "resolve", "--json"]
    );

    let (code, envelope) = profile.json(&["space", "list", "--json"]);
    assert_eq!(code, 4, "{envelope:#}");
    assert_eq!(envelope["command"], "space list");
    assert_eq!(envelope["error"]["code"], "auth_required");

    let (code, envelope) = profile.json(&["--json", "folder", "retry", "dir"]);
    assert_eq!(code, 2, "{envelope:#}");
    assert_eq!(envelope["command"], "folder retry");
    assert_eq!(envelope["error"]["code"], "usage");

    let (code, envelope) = profile.json(&["history", "versions", "a.txt", "--json"]);
    assert_eq!(code, 4, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "auth_required");
    assert_eq!(argv(&envelope["next_commands"][0]), ["koi", "setup"]);

    let (code, envelope) = profile.json(&["status", "--json=2"]);
    assert_eq!(code, 2, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "usage");
}

#[test]
fn a_share_password_is_the_first_line_of_stdin_and_a_short_one_names_its_reason() {
    use std::io::Write;

    let profile = Profile::new();
    let file = profile.home.path().join("report.docx");
    let share = |stdin: &str| {
        let mut child = (profile.command(&["--json", "share", "create", "--password", "--"]))
            .arg(&file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        schema::check(&envelope);
        (output.status.code().unwrap(), envelope)
    };

    let (code, envelope) = share("short\nlong enough password\n");
    assert_eq!(code, 2, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "usage");
    assert_eq!(envelope["error"]["reason"], "password_length");

    let (code, envelope) = share("long enough password\nshort\n");
    assert_eq!(code, 4, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "auth_required");
    assert!(envelope["error"].get("reason").is_none(), "{envelope:#}");
}

#[test]
fn unlinking_a_folder_that_is_not_linked_is_unchanged() {
    let profile = Profile::new();
    let (code, envelope) = profile.json(&["folder", "unlink", "Docs", "--json"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    assert_eq!(envelope["error"], Value::Null);
    let path = (std::fs::canonicalize(profile.home.path()).unwrap())
        .join("Docs")
        .display()
        .to_string();
    let mut data = envelope["data"].clone();
    data.as_object_mut().unwrap().remove("service");
    assert_eq!(data, serde_json::json!({ "path": path }));
    let output = profile.output(&["folder", "unlink", "Docs"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{path} is not linked, so there is nothing to unlink.\n")
    );
}

#[test]
fn a_change_with_a_request_id_that_failed_records_nothing() {
    let profile = Profile::new();
    let requests = profile.home.path().join("state/koidrive/requests");
    for args in [
        &["folder", "restart", "Docs", "--request-id", "r1", "--json"][..],
        &["folder", "restart", "Docs", "--request-id", "r1", "--json"],
        &[
            "folder",
            "retry",
            "Docs",
            "--resend",
            "--request-id",
            "r2",
            "--json",
        ],
    ] {
        let (code, envelope) = profile.json(args);
        assert_eq!(code, 2, "{args:?}: {envelope:#}");
        assert!(
            (envelope["error"]["message"].as_str().unwrap()).contains("is not a linked folder"),
            "{envelope:#}"
        );
        let left: Vec<_> = std::fs::read_dir(&requests)
            .map(|entries| entries.map(|entry| entry.unwrap().file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
    }
    let (code, envelope) =
        profile.json(&["folder", "restart", "Docs", "--request-id", "a/b", "--json"]);
    assert_eq!(
        (code, &envelope["error"]["code"]),
        (2, &Value::from("usage"))
    );
}

#[test]
fn help_and_the_version_are_envelopes_under_json() {
    let profile = Profile::new();
    let (code, envelope) = profile.json(&["--json"]);
    assert_eq!(code, 0);
    assert_eq!(envelope["command"], "");
    assert!(
        envelope["data"]["help"]
            .as_str()
            .unwrap()
            .contains("Usage: koi"),
        "{envelope:#}"
    );
    let (code, envelope) = profile.json(&["folder", "--json"]);
    assert_eq!((code, &envelope["command"]), (0, &Value::from("folder")));
    let help = envelope["data"]["help"].as_str().unwrap();
    assert!(help.contains("Usage: koi folder"), "{help}");
    assert!(help.contains("retry"), "{help}");
    let (code, envelope) = profile.json(&["history", "--help", "--json"]);
    assert_eq!(code, 0);
    assert!(
        envelope["data"]["help"]
            .as_str()
            .unwrap()
            .contains("requests"),
        "{envelope:#}"
    );
    let (code, envelope) = profile.json(&["help", "--json"]);
    assert_eq!((code, &envelope["command"]), (0, &Value::from("help")));
    assert!(
        (envelope["data"]["help"].as_str().unwrap()).contains("Usage: koi"),
        "{envelope:#}"
    );
    let (code, envelope) = profile.json(&["help", "folder", "--json"]);
    assert_eq!(
        (code, &envelope["command"]),
        (0, &Value::from("help folder"))
    );
    let help = envelope["data"]["help"].as_str().unwrap();
    assert!(help.contains("Usage: koi folder"), "{help}");
    let (code, envelope) = profile.json(&["--json", "help", "folder", "link"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert!(
        (envelope["data"]["help"].as_str().unwrap()).contains("Usage: koi folder link"),
        "{envelope:#}"
    );
    for args in [
        &["help", "--json=2"][..],
        &["help", "folder", "--json=bogus"],
        &["--help", "--json=2"],
        &["folder", "--help", "--json=2"],
        &["--version", "--json=2"],
    ] {
        let (code, envelope) = profile.json(args);
        assert_eq!(code, 2, "{args:?}: {envelope:#}");
        assert_eq!(envelope["error"]["code"], "usage", "{args:?}");
    }
    let (code, envelope) = profile.json(&["help", "nothing", "--json"]);
    assert_eq!(
        (code, &envelope["error"]["code"]),
        (2, &Value::from("usage"))
    );
    let (code, envelope) = profile.json(&["--version", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(envelope["data"]["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn setup_without_a_terminal_lists_the_choices_and_asks_nothing() {
    let profile = Profile::new();
    let output = profile.output(&["setup"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("koi join <CODE>"), "{text}");
    assert!(text.contains("koi server deploy --interactive"), "{text}");

    let (code, envelope) = profile.json(&["setup", "--json"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(envelope["data"]["choices"].as_array().unwrap().len(), 5);
    assert_eq!(
        argv(&envelope["next_commands"][1]),
        ["koi", "server", "deploy", "--interactive"]
    );

    let output = profile.output(&["setup", "--interactive"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("needs a terminal"), "{stderr}");
    let (code, envelope) = profile.json(&["setup", "--interactive", "--json"]);
    assert_eq!(code, 2, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "usage");
}

#[test]
fn owner_login_commands_never_log_in_under_json_or_without_their_flags() {
    let profile = Profile::new();
    let (code, envelope) = profile.json(&["server", "deploy", "--json"]);
    assert_eq!(code, 4, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "auth_required");
    assert_eq!(
        argv(&envelope["next_commands"][0]),
        ["koi", "server", "deploy", "--interactive"]
    );
    let target = [
        "server",
        "delete",
        "--bucket",
        "b",
        "--jurisdiction",
        "eu",
        "--yes",
        "--no-browser",
        "--port",
        "8976",
    ];
    let (code, envelope) = profile.json(&[&["--json=1"][..], &target].concat());
    assert_eq!(code, 4, "{envelope:#}");
    assert_eq!(
        argv(&envelope["next_commands"][0]),
        [
            "koi",
            "server",
            "delete",
            "--bucket",
            "b",
            "--jurisdiction",
            "eu",
            "--interactive",
            "--no-browser",
            "--port",
            "8976"
        ]
    );
    let (code, envelope) = profile.json(&["server", "deploy", "--name", "desk", "--json"]);
    assert_eq!(code, 4, "{envelope:#}");
    assert_eq!(
        argv(&envelope["next_commands"][0]),
        ["koi", "server", "deploy", "--name", "desk", "--interactive"]
    );

    let pasted = profile.output(&["server", "add-device", "--no-browser"]);
    assert_eq!(pasted.status.code(), Some(4));
    for args in [
        &["server", "deploy"][..],
        &["server", "upgrade", "--worker", "koidrive", "--yes"],
        &["server", "delete", "--yes"],
        &["server", "add-device", "--interactive"],
    ] {
        let output = profile.output(args);
        assert_eq!(output.status.code(), Some(4), "{args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("--interactive on a terminal"),
            "{args:?}: {stderr}"
        );
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn a_path_echoed_in_an_error_reaches_stderr_escaped() {
    let profile = Profile::new();
    let typed = profile.home.path().join("\u{1b}]0;owned\u{7}notes\n.txt");
    let output = profile.output(&["folder", "restart", typed.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.contains('\u{1b}') && !stderr.contains('\u{7}'),
        "{stderr:?}"
    );
    assert!(
        stderr.contains("\\u{1b}]0;owned\\u{7}notes\\n.txt\" is not a linked folder"),
        "{stderr:?}"
    );
    assert_eq!(stderr.lines().count(), 1, "{stderr:?}");

    let output = profile.output(&["folder", "unlink", "x\u{1b}[31m\nFORGED"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
    assert!(stdout.contains("x\\u{1b}[31m\\u{a}FORGED"), "{stdout:?}");

    for args in [
        &["status", "--bad\nFORGED\u{1b}[2J"][..],
        &["device", "revoke", "x\nFORGED"],
    ] {
        let output = profile.output(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            !stderr.contains('\u{1b}') && !stderr.lines().any(|line| line.starts_with("FORGED")),
            "{args:?}: {stderr}"
        );
        assert!(stderr.contains("\\u{a}FORGED"), "{args:?}: {stderr}");
    }
}

#[test]
fn status_and_resolve_on_an_empty_home_create_nothing() {
    let profile = Profile::new();
    let (code, envelope) = profile.json(&["status", "--json"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(envelope["data"]["account"], Value::Null);
    assert_eq!(envelope["data"]["running"], false);
    let (code, envelope) = profile.json(&["resolve", "--json"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(envelope["decisions"], serde_json::json!([]));
    let output = profile.output(&["status"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let created: Vec<_> = std::fs::read_dir(profile.home.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(created.is_empty(), "{created:?}");
}

#[test]
fn ctrl_c_ends_a_command_with_an_interrupted_error_and_exit_130() {
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}", silent.local_addr().unwrap());
    for json in [true, false] {
        let profile = Profile::new();
        let mut args = vec!["join", "K7QM-2XPD", "--server", &server];
        if json {
            args.push("--json");
        }
        let child = (profile.command(&args))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (_held, _) = silent.accept().expect("the join calls the server");
        let killed = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        let output = child.wait_with_output().unwrap();

        assert_eq!(output.status.code(), Some(130), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        if json {
            let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
            schema::check(&envelope);
            assert_eq!(envelope["command"], "join", "{envelope:#}");
            assert_eq!(envelope["outcome"], "error", "{envelope:#}");
            assert_eq!(envelope["error"]["code"], "interrupted", "{envelope:#}");
            assert_eq!(envelope["error"]["retryable"], false, "{envelope:#}");
        } else {
            assert!(output.stdout.is_empty(), "{stderr}");
            assert!(stderr.contains("koi: interrupted"), "{stderr}");
        }
    }
}

#[test]
fn ctrl_c_during_setup_under_json_prints_the_interrupted_envelope() {
    let profile = Profile::new();
    let home = profile.home.path();
    let socket = koidrive_driver::Paths::from_vars(|name| match name {
        "XDG_CONFIG_HOME" => Some(home.join("config").into()),
        "XDG_STATE_HOME" => Some(home.join("state").into()),
        "XDG_RUNTIME_DIR" => Some(home.join("run").into()),
        _ => None,
    })
    .unwrap()
    .socket();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let silent = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let child = (profile.command(&["setup", "--json"]))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (_held, _) = silent
        .accept()
        .expect("setup asks the daemon for decisions");
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(130), "{output:?}");
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    schema::check(&envelope);
    assert_eq!(envelope["command"], "setup", "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "interrupted", "{envelope:#}");
}

#[test]
fn a_help_or_version_flag_before_a_subcommand_reports_the_command_it_belongs_to() {
    let profile = Profile::new();
    for (args, command, field) in [
        (&["--version", "space", "list"][..], "", "version"),
        (&["-V", "folder", "link"], "", "version"),
        (&["-Vh", "space", "list"], "", "version"),
        (&["-hV", "space", "list"], "", "help"),
        (&["-h", "space", "list"], "", "help"),
        (&["space", "--help", "list"], "space", "help"),
    ] {
        let (code, envelope) = profile.json(&[&["--json"], args].concat());
        assert_eq!(code, 0, "{args:?} {envelope:#}");
        assert_eq!(envelope["command"], command, "{args:?} {envelope:#}");
        assert!(envelope["data"][field].is_string(), "{args:?} {envelope:#}");
        schema::check(&envelope);
    }
}

#[test]
fn help_named_after_a_group_is_the_help_of_the_commands_after_it() {
    let profile = Profile::new();
    for (args, command, shown) in [
        (
            &["space", "help"][..],
            "help space",
            &["space", "--help"][..],
        ),
        (
            &["space", "help", "list"],
            "help space list",
            &["space", "list", "--help"],
        ),
        (
            &["folder", "help", "-h"],
            "help folder",
            &["folder", "--help"],
        ),
        (&["help", "space"], "help space", &["space", "--help"]),
    ] {
        let (code, envelope) = profile.json(&[args, &["--json"]].concat());
        assert_eq!(code, 0, "{args:?} {envelope:#}");
        assert_eq!(envelope["command"], command, "{args:?} {envelope:#}");
        let (_, expected) = profile.json(&[shown, &["--json"]].concat());
        assert_eq!(
            envelope["data"]["help"], expected["data"]["help"],
            "{args:?}"
        );
    }
    let (code, envelope) = profile.json(&["folder", "link", "help", "--json"]);
    assert_eq!(code, 2, "{envelope:#}");
    assert_eq!(envelope["command"], "folder link", "{envelope:#}");
    for (args, field) in [
        (&["--version", "space", "help"][..], "version"),
        (&["--help", "space", "help"], "help"),
    ] {
        let (code, envelope) = profile.json(&[args, &["--json"]].concat());
        assert_eq!(code, 0, "{args:?} {envelope:#}");
        assert_eq!(envelope["command"], "", "{args:?} {envelope:#}");
        assert!(envelope["data"][field].is_string(), "{args:?} {envelope:#}");
    }
    let (code, envelope) = profile.json(&["space", "--bogus", "help", "--json"]);
    assert_eq!(code, 2, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "usage", "{envelope:#}");
}

#[test]
fn every_command_prints_its_help_as_an_envelope_its_schema_takes() {
    let profile = Profile::new();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/json-schema/v1");
    let mut commands = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let schema: Value =
            serde_json::from_slice(&std::fs::read(entry.unwrap().path()).unwrap()).unwrap();
        if let Some(command) = schema["properties"]["command"]["const"].as_str() {
            commands.push(command.to_owned());
        }
    }
    assert!(commands.len() > 30, "{commands:?}");
    for command in commands {
        let mut args: Vec<&str> = command.split_whitespace().collect();
        args.extend(["--help", "--json"]);
        let (code, envelope) = profile.json(&args);
        assert_eq!(code, 0, "{envelope:#}");
        assert_eq!(envelope["command"], command.as_str(), "{envelope:#}");
        assert!(envelope["data"]["help"].is_string(), "{envelope:#}");
        assert_eq!(
            envelope["data"]["commands"][0]["command"],
            command.as_str(),
            "{envelope:#}"
        );
    }
}

#[test]
fn the_help_manifest_names_each_command_its_schema_and_the_sign_in_it_needs() {
    let profile = Profile::new();
    std::fs::create_dir(profile.home.path().join("x")).unwrap();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/json-schema/v1");
    let (_, help) = profile.json(&["help", "--json"]);
    let mut checked = Vec::new();
    for info in help["data"]["commands"].as_array().unwrap() {
        let command = info["command"].as_str().unwrap();
        assert!(
            dir.join(info["schema"].as_str().unwrap()).is_file(),
            "{info:#}"
        );
        let group = !info["subcommands"].as_array().unwrap().is_empty();
        if group || matches!(command, "help" | "service start" | "_daemon") {
            continue;
        }
        assert_eq!(info["output"], "envelope", "{info:#}");
        let named = |list: &str| -> Vec<(String, Value)> {
            let key = match list {
                "arguments" => "name",
                _ => "long",
            };
            (info[list].as_array().unwrap().iter())
                .map(|item| match list {
                    "arguments" => (item[key].as_str().unwrap().to_owned(), item.clone()),
                    _ => (format!("--{}", item[key].as_str().unwrap()), item.clone()),
                })
                .collect()
        };
        let all: Vec<(String, Value)> = [named("arguments"), named("options")].concat();
        let mut wanted: Vec<String> = (all.iter())
            .filter(|(_, item)| item["required"] == true)
            .map(|(name, _)| name.clone())
            .collect();
        for group in info["groups"].as_array().unwrap() {
            if group["required"] == true {
                wanted.push(group["members"][0].as_str().unwrap().to_owned());
            }
        }
        let mut next = 0;
        while next < wanted.len() {
            let item = &all
                .iter()
                .find(|(name, _)| *name == wanted[next])
                .unwrap()
                .1;
            for name in item["requires"].as_array().unwrap() {
                let name = name.as_str().unwrap().to_owned();
                if !wanted.contains(&name) {
                    wanted.push(name);
                }
            }
            next += 1;
        }
        let mut envelope = Value::Null;
        for value in ["x", "1", "0190f0a0-0000-7000-8000-00000000000a"] {
            let mut args: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
            for (name, item) in all.iter().filter(|(name, _)| wanted.contains(name)) {
                let sample = (item["values"].as_array())
                    .and_then(|values| values.first())
                    .and_then(Value::as_str)
                    .unwrap_or(value)
                    .to_owned();
                match (name.starts_with("--"), item["value"].is_string()) {
                    (false, _) => args.push(sample),
                    (true, false) => args.push(name.clone()),
                    (true, true) => args.extend([name.clone(), sample]),
                }
            }
            args.push("--json".to_owned());
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let (code, printed) = profile.json(&args);
            envelope = printed;
            let message = envelope["error"]["message"].as_str().unwrap_or_default();
            if !(code == 2 && message.contains("invalid value")) {
                break;
            }
        }
        let next = envelope["next_commands"].as_array().unwrap();
        match info["auth"].as_str().unwrap() {
            "device" => {
                assert_eq!(envelope["error"]["code"], "auth_required", "{envelope:#}");
                assert_eq!(argv(&next[0]), ["koi", "setup"], "{envelope:#}");
            }
            "cloudflare" => {
                assert_eq!(envelope["error"]["code"], "auth_required", "{envelope:#}");
                assert_eq!(
                    argv(&next[0]).last(),
                    Some(&"--interactive"),
                    "{envelope:#}"
                );
            }
            _ => assert_ne!(envelope["error"]["code"], "auth_required", "{envelope:#}"),
        }
        checked.push(command.to_owned());
    }
    for command in [
        "status",
        "space list",
        "folder link",
        "server deploy",
        "telemetry on",
        "history versions",
        "device revoke",
    ] {
        assert!(
            checked.iter().any(|checked| checked == command),
            "{checked:?}"
        );
    }
}
