// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use koidrive_test_support::Sandbox;

#[path = "support/process.rs"]
mod process;

const CHILD: &str = "KOI_SANDBOX_TEST_CHILD";

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const ENDED: Duration = Duration::from_secs(30);

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + ENDED;
    while !done() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn daemon(sandbox: &Sandbox) -> (Daemon, PathBuf) {
    let home = sandbox.path();
    let mut child = process::koi(home)
        .args(["_daemon", "--json"])
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("KOIDRIVE_TELEMETRY", "0")
        .env("KOIDRIVE_UPDATE_CHECK", "0")
        .env_remove("KOIDRIVE_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut started = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut started)
        .unwrap();
    let daemon = Daemon(child);
    let started: serde_json::Value = serde_json::from_str(&started).unwrap();
    let socket = PathBuf::from(started["socket"].as_str().unwrap());
    (daemon, socket)
}

#[test]
fn a_koi_stops_when_its_sandbox_drops_after_its_test_passes_or_panics() {
    for panics in [false, true] {
        let sandbox = Sandbox::new();
        let root = sandbox.path().to_path_buf();
        let (mut daemon, _) = daemon(&sandbox);
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _sandbox = sandbox;
            assert!(!panics, "the test failed");
        }));
        assert_eq!(ended.is_err(), panics);
        let mut status = None;
        until("the daemon outlived its sandbox", || {
            status = daemon.0.try_wait().unwrap();
            status.is_some()
        });
        assert_eq!(
            status.and_then(|status| status.signal()),
            Some(9),
            "{status:?}"
        );
        assert!(!root.exists());
    }
}

struct Left {
    pid: rustix::process::Pid,
    lifeline: PathBuf,
}

impl Drop for Left {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process(self.pid, rustix::process::Signal::KILL);
        if let Ok(root) = std::fs::read_to_string(self.lifeline.join("root")) {
            let _ = std::fs::remove_dir_all(root);
        }
        let _ = std::fs::remove_dir_all(&self.lifeline);
    }
}

#[test]
fn a_koi_stops_when_the_test_process_that_started_it_is_killed() {
    if std::env::var_os(CHILD).is_some() {
        let sandbox = Sandbox::new();
        let (daemon, socket) = daemon(&sandbox);
        println!(
            "{CHILD} {} {} {}",
            daemon.0.id(),
            sandbox.lifeline().display(),
            socket.display()
        );
        std::io::stdout().flush().unwrap();
        loop {
            std::thread::park();
        }
    }
    let mut test = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_koi_stops_when_the_test_process_that_started_it_is_killed",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let line = BufReader::new(test.stdout.take().unwrap())
        .lines()
        .map(Result::unwrap)
        .find_map(|line| line.strip_prefix(&format!("{CHILD} ")).map(str::to_owned))
        .unwrap();
    let mut words = line.split(' ');
    let pid: i32 = words.next().unwrap().parse().unwrap();
    let _left = Left {
        pid: rustix::process::Pid::from_raw(pid).unwrap(),
        lifeline: PathBuf::from(words.next().unwrap()),
    };
    let socket = PathBuf::from(words.next().unwrap());
    let answered = UnixStream::connect(&socket).is_ok();

    test.kill().unwrap();
    test.wait().unwrap();

    assert!(answered, "the daemon was not running");
    until(
        "the daemon still answers after the test process that started it was killed",
        || UnixStream::connect(&socket).is_err(),
    );
}

#[test]
fn a_test_cannot_run_koi_against_this_account_s_own_home() {
    let account = std::env::home_dir().unwrap();
    let sandbox = Sandbox::new();
    for (name, value) in [
        ("HOME", account.clone()),
        ("XDG_STATE_HOME", account.join(".local/state")),
        ("XDG_CONFIG_HOME", account.join(".config")),
    ] {
        let output = process::koi(sandbox.path())
            .args(["help", "--json"])
            .env("HOME", sandbox.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env(name, &value)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(78), "{name}: {stderr}");
        assert!(
            stderr.contains("in this account's home"),
            "{name}: {stderr}"
        );
    }
}

#[test]
fn a_koi_outside_a_sandbox_cannot_start() {
    let outside = tempfile::tempdir().unwrap();
    let started = std::panic::catch_unwind(|| process::koi(outside.path()));
    assert!(started.is_err());
}

const ALLOWED: [&str; 7] = [
    "\"kill\"",
    "\"/bin/kill\"",
    "\"/bin/cp\"",
    "\"/bin/launchctl\"",
    "\"systemctl\"",
    "\"pgrep\"",
    "std::env::current_exe()",
];

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        match path.is_dir() {
            true => sources(&path, found),
            false if path.extension().is_some_and(|extension| extension == "rs") => {
                found.push(path)
            }
            false => {}
        }
    }
}

#[test]
fn every_program_a_test_runs_that_could_start_koi_is_sandboxed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    sources(&root.join("cli/tests"), &mut files);
    files.push(root.join("core/ffi/src/daemon/server.rs"));
    let mut unsandboxed = Vec::new();
    let support = root.join("cli/tests/support");
    for file in files.iter().filter(|file| !file.starts_with(&support)) {
        let text = std::fs::read_to_string(file).unwrap();
        for (number, line) in text.lines().enumerate() {
            let spawns = line
                .match_indices(concat!("Command", "::new("))
                .any(|(at, call)| {
                    let program = &line[at + call.len()..];
                    !ALLOWED.iter().any(|allowed| program.starts_with(allowed))
                });
            if spawns || line.contains(concat!("CARGO_BIN_", "EXE_koi")) {
                unsandboxed.push(format!(
                    "{}:{}: {}",
                    file.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        unsandboxed.is_empty(),
        "run these with process::koi or koidrive_test_support::sandboxed, so that a koi they start stops with the test:\n{}",
        unsandboxed.join("\n")
    );
}
