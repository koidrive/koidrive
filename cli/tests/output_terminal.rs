// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Profile(tempfile::TempDir);

struct Capture {
    exit: i32,
    stdout: String,
    stderr: String,
    terminal: String,
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
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("koi {args:?} did not exit");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        Capture {
            exit: output.status.code().unwrap(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
            terminal: reader
                .map(|reader| reader.join().unwrap())
                .unwrap_or_default(),
        }
    }

    fn fixture(
        &self,
        count: usize,
        reply: impl Fn(Value) -> Value + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let state = self.0.path().join("state/koidrive");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let listener = UnixListener::bind(state.join("daemon.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
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
        })
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
        "■  this device is not set up; run koi setup first\n"
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
            .starts_with("■  error: unrecognized subcommand 'doctor'\n")
    );
    assert!(tty.terminal.contains("│  Usage: koi [OPTIONS] [COMMAND]\n"));
}

#[test]
fn a_color_terminal_uses_the_setup_colors() {
    let output = Profile::new().run(&["resolve"], true, false, &[("CLICOLOR_FORCE", "1")]);
    assert!(output.terminal.contains('\u{1b}'));
    assert!(output.terminal.contains("No decisions are waiting."));
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
            "koi: this device is not set up; run koi setup first\n"
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
