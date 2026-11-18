// SPDX-License-Identifier: Apache-2.0

#[test]
fn update_usage_keeps_main_rendering_without_changing_its_error_prefix() {
    let output = Profile::new().run(
        &["update", "--unknown-fixture-flag"],
        true,
        false,
        &[("NO_COLOR", "1")],
    );
    assert_eq!(output.exit, 2);
    assert!(
        output
            .terminal
            .contains("error: unexpected argument '--unknown-fixture-flag' found"),
        "{}",
        output.terminal
    );
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
fn unreadable_telemetry_settings_keep_parser_details_out_of_terminal_output() {
    for (tty, json_output) in [(true, false), (false, false), (false, true)] {
        let profile = Profile::new();
        let directory = profile.0.path().join("state/koidrive/telemetry");
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(
            directory.parent().unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::write(directory.join("settings.json"), "{broken").unwrap();
        let mut args = vec!["telemetry", "status"];
        if json_output {
            args.push("--json");
        }
        let output = profile.run(&args, tty, false, &[("NO_COLOR", "1")]);
        assert_eq!(output.exit, 1);
        if tty {
            assert!(
                output
                    .terminal
                    .contains("could not read saved usage-report data"),
                "{}",
                output.terminal
            );
            assert!(
                output.terminal.contains("koi diagnose"),
                "{}",
                output.terminal
            );
            assert!(
                !output
                    .terminal
                    .contains(&profile.0.path().display().to_string()),
                "{}",
                output.terminal
            );
            assert!(!output.terminal.contains("line 1"), "{}", output.terminal);
        } else if json_output {
            let envelope: Value = serde_json::from_str(&output.stdout).unwrap();
            assert!(
                envelope["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("the telemetry files in")
            );
            assert!(envelope["error"].get("human").is_none());
        } else {
            assert!(
                output.stderr.contains("the telemetry files in"),
                "{}",
                output.stderr
            );
            assert!(output.stderr.contains("line 1"), "{}", output.stderr);
        }
    }
}

#[test]
fn an_empty_daemon_human_message_cannot_hide_a_failure() {
    let profile = Profile::new();
    let server = profile.fixture(1, |_| json!({"response":"error", "message":"raw daemon message", "human":"", "code":"failed"}));
    let output = profile.run(&["pause"], true, false, &[("NO_COLOR", "1")]);
    server.join().unwrap();
    assert_eq!(output.exit, 1);
    assert!(
        output.terminal.contains("raw daemon message"),
        "{}",
        output.terminal
    );
}

#[test]
fn read_only_lists_offer_retry_without_unknown_mutation_guidance() {
    for args in [["space", "list"], ["device", "list"], ["share", "list"]] {
        let profile = Profile::new();
        let server = profile.http(503, Vec::new());
        let output = profile.run(&args, true, false, &[("NO_COLOR", "1"), ("NO_PROXY", "*")]);
        server.join().unwrap();
        assert_eq!(output.exit, 6);
        assert!(
            output
                .terminal
                .contains("Wait before trying this command again"),
            "{}",
            output.terminal
        );
        assert!(!output.terminal.contains("inspect"), "{}", output.terminal);
    }
}

#[test]
fn joining_a_busy_server_offers_retry_without_a_saved_sign_in() {
    let profile = Profile::new();
    let fixture = profile.http_responses(vec![("POST /v1/pair/complete ", 503, Vec::new()); 4]);
    let credential = profile.0.path().join("state/koidrive/credential.json");
    let saved: Value = serde_json::from_slice(&fs::read(&credential).unwrap()).unwrap();
    let server = saved["server"].as_str().unwrap();
    fs::remove_file(&credential).unwrap();
    let output = profile.run(
        &["join", "ABCD-EFGH", "--server", server, "--name", "Laptop"],
        true,
        false,
        &[("NO_COLOR", "1"), ("NO_PROXY", "*")],
    );
    fixture.join().unwrap();
    assert_eq!(output.exit, 6);
    assert!(
        output
            .terminal
            .contains("Wait before trying this command again"),
        "{}",
        output.terminal
    );
    assert!(!output.terminal.contains("inspect"), "{}", output.terminal);
    assert!(
        !output.terminal.contains("koi device list"),
        "{}",
        output.terminal
    );
    assert!(!credential.exists());
}

#[test]
fn joining_an_unreachable_server_does_not_require_a_connected_device() {
    let profile = Profile::new();
    let server = format!("http://{}/", koidrive_test_support::reset_tcp_address());
    let output = profile.run(
        &["join", "ABCD-EFGH", "--server", &server, "--name", "Laptop"],
        true,
        false,
        &[("NO_COLOR", "1"), ("NO_PROXY", "*")],
    );
    assert_eq!(output.exit, 6);
    assert!(
        output
            .terminal
            .contains("Check your connection and the server address, then try again"),
        "{}",
        output.terminal
    );
    assert!(!output.terminal.contains("inspect"), "{}", output.terminal);
    assert!(
        !output.terminal.contains("koi device list"),
        "{}",
        output.terminal
    );
    assert!(
        !profile
            .0
            .path()
            .join("state/koidrive/credential.json")
            .exists()
    );
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
        if !diagnostic {
            assert!(
                text.contains("Run this command with --verbose for details"),
                "{text}"
            );
        }
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
            "The server refused the request: The owner disabled this operation",
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
                assert!(!text.contains("Before repeating a change"), "{text}");
                if status == 503 {
                    assert!(
                        text.contains("Wait before trying this command again"),
                        "{text}"
                    );
                } else if status == 200 {
                    assert!(text.contains("--verbose for details"), "{text}");
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
        let server = format!("http://{}/", koidrive_test_support::reset_tcp_address());
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
                text.contains("Koidrive could not reach the server. Check your connection and the server address, then try again."),
                "{text}"
            );
            assert!(!text.contains("Before repeating a change"), "{text}");
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
            "The server refused the request: History access was removed",
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
        let warning = match extra {
            Some("--json") => "\"unconfirmed\":",
            _ => "The server did not confirm that it revoked this device",
        };
        assert!(text.contains(warning), "{text}");
        if extra != Some("--json") {
            assert!(text.contains("koi device revoke device-internal"), "{text}");
        }
        assert!(!text.contains("koidrive_driver::driver"), "{text}");
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
