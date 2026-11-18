// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use koidrive_test_support::Sandbox;

mod schema;

#[path = "support/process.rs"]
mod process;

#[test]
fn logs_to_a_pipe_are_plain_text() {
    let home = Sandbox::new();
    let mut daemon = process::koi(home.path())
        .arg("_daemon")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_RUNTIME_DIR", home.path().join("run"))
        .env_remove("NO_COLOR")
        .env_remove("KOIDRIVE_LOG")
        .env("KOIDRIVE_UPDATE_CHECK", "0")
        .stderr(Stdio::piped())
        .spawn()
        .expect("run koi");
    let mut log = String::new();
    BufReader::new(daemon.stderr.take().unwrap())
        .read_line(&mut log)
        .unwrap();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(log.contains("the daemon is running"), "{log}");
    assert!(!log.contains('\u{1b}'), "{log:?}");
}

#[test]
fn the_daemon_under_json_prints_its_lifecycle_as_json_lines() {
    let home = Sandbox::new();
    let daemon = |json: bool| {
        let mut command = process::koi(home.path());
        command
            .arg("_daemon")
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("XDG_STATE_HOME", home.path().join("state"))
            .env("XDG_RUNTIME_DIR", home.path().join("run"))
            .env("CLICOLOR_FORCE", "1")
            .env_remove("NO_COLOR")
            .env_remove("KOIDRIVE_LOG")
            .env("KOIDRIVE_UPDATE_CHECK", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if json {
            command.arg("--json");
        }
        command
    };
    let mut running = daemon(true).spawn().expect("run koi");
    let pid = running.id().to_string();
    let (_finished, finish) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let waited = finish.recv_timeout(std::time::Duration::from_secs(30));
        if waited == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            let _ = Command::new("kill").args(["-KILL", &pid]).status();
        }
    });
    let mut stdout = BufReader::new(running.stdout.take().unwrap());
    let mut started = String::new();
    stdout.read_line(&mut started).unwrap();
    let started: serde_json::Value = serde_json::from_str(&started).unwrap();
    assert_eq!(started["schema_version"], 1, "{started}");
    assert_eq!(started["event"], "started", "{started}");
    assert_eq!(started["folders"], 0, "{started}");
    schema::check(&started);

    let help = daemon(true).arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0), "{help:?}");
    let help = String::from_utf8(help.stdout).unwrap();
    assert_eq!(help.lines().count(), 1, "{help}");
    let envelope: serde_json::Value = serde_json::from_str(&help).unwrap();
    schema::check(&envelope);
    assert!(
        (envelope["data"]["help"].as_str().unwrap()).contains("_daemon"),
        "{envelope}"
    );
    assert_eq!(
        envelope["data"]["commands"][0]["output"], "lines",
        "{envelope}"
    );

    let busy = daemon(true).output().unwrap();
    assert_eq!(busy.status.code(), Some(1), "{busy:?}");
    let busy = String::from_utf8(busy.stdout).unwrap();
    assert_eq!(busy.lines().count(), 1, "{busy}");
    let envelope: serde_json::Value = serde_json::from_str(&busy).unwrap();
    schema::check(&envelope);
    assert_eq!(envelope["error"]["code"], "failed", "{envelope}");

    let killed = Command::new("kill")
        .args(["-TERM", &running.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let output = running.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let rest: Vec<String> = stdout.lines().map(Result::unwrap).collect();
    let events: Vec<serde_json::Value> = (rest.iter())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), 2, "{rest:?}");
    assert_eq!(events[0]["event"], "stopping");
    assert_eq!(events[0]["signal"], "SIGTERM");
    assert_eq!(events[1]["event"], "stopped");
    assert!(events.iter().all(|event| event["schema_version"] == 1));
    events.iter().for_each(schema::check);

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    for line in stderr.lines() {
        let log: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("{line:?} is not a JSON log line: {error}"));
        assert!(log.get("level").is_some(), "{line}");
    }
}

#[test]
fn profiles_that_share_a_runtime_directory_run_daemons_side_by_side() {
    let home = Sandbox::new();
    let start = |profile: &str| {
        let mut daemon = process::koi(home.path())
            .args(["_daemon", "--json"])
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(profile).join("config"))
            .env("XDG_STATE_HOME", home.path().join(profile).join("state"))
            .env("XDG_RUNTIME_DIR", home.path().join("run"))
            .env_remove("KOIDRIVE_LOG")
            .env("KOIDRIVE_UPDATE_CHECK", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("run koi");
        let mut stdout = BufReader::new(daemon.stdout.take().unwrap());
        let mut started = String::new();
        stdout.read_line(&mut started).unwrap();
        let started: serde_json::Value = serde_json::from_str(&started).unwrap();
        let socket = started["socket"].as_str().unwrap().to_owned();
        (daemon, stdout, socket)
    };
    let (mut work, _work_out, work_socket) = start("work");
    let (mut personal, _personal_out, personal_socket) = start("personal");
    assert_ne!(work_socket, personal_socket);
    for socket in [&work_socket, &personal_socket] {
        assert!(std::path::Path::new(socket).exists(), "{socket}");
    }
    for daemon in [&mut work, &mut personal] {
        daemon.kill().unwrap();
        daemon.wait().unwrap();
    }
}
