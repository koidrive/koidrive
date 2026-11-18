// SPDX-License-Identifier: Apache-2.0

#[test]
fn terminal_results_use_the_setup_theme() {
    let output = Profile::new().run(&["resolve"], true, false, &[("NO_COLOR", "1")]);
    assert_eq!(output.exit, 0);
    assert_eq!(output.terminal, "●  No decisions are waiting.\n");
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
