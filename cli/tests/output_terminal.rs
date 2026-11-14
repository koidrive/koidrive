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
            .starts_with("▲  Device: not set up; run koi setup\n")
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
        output.terminal.starts_with("▲  Device: Laptop"),
        "{}",
        output.terminal
    );
    assert!(output.terminal.contains("Halted"));
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
