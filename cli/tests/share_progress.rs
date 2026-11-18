// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use koidrive_test_support::Sandbox;

#[path = "support/process.rs"]
mod process;

#[test]
fn a_slow_share_request_reports_its_wait_on_stderr_without_changing_json_stdout() {
    let home = Sandbox::in_dir(std::path::Path::new("/tmp"));
    let state = home.path().join("state/koidrive");
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(state.join("daemon.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        for _ in 0..2 {
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "the CLI did not send its request"
                        );
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
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let response = match request["request"].as_str().unwrap() {
                "share" => {
                    std::thread::sleep(Duration::from_millis(1200));
                    serde_json::json!({"response":"shared", "link":{
                        "id":"0123456789", "url":"https://files.example.com/s/AAAAAAAAAAAAAAAAAAAAAA",
                        "space_id":"s1", "space_name":"Team", "folder":null, "node_id":"n1",
                        "target":{"path":"notes.txt", "kind":"file"}, "created_at_ms":1000,
                        "expires_at_ms":null, "revoked_at_ms":null, "has_password":false, "state":"active"
                    }})
                }
                "questions" => serde_json::json!({"response":"questions", "questions":[]}),
                other => panic!("unexpected request {other}"),
            };
            writeln!(stream, "{response}").unwrap();
        }
    });
    let output = process::koi(home.path())
        .args(["share", "create", "notes.txt", "--json"])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env_remove("XDG_RUNTIME_DIR")
        .env("DO_NOT_TRACK", "1")
        .output()
        .unwrap();
    assert!(output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["outcome"], "ok");
    assert_eq!(
        envelope["data"]["url"],
        "https://files.example.com/s/AAAAAAAAAAAAAAAAAAAAAA"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr.matches("Checking files before sharing...").count(),
        1
    );
    assert!(!stderr.contains("AAAAAAAAAAAAAAAAAAAAAA"));
    server.join().unwrap();
}
