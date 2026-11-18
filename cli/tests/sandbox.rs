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

struct Left(rustix::process::Pid);

impl Drop for Left {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process(self.0, rustix::process::Signal::KILL);
    }
}

#[test]
fn a_koi_stops_when_the_test_process_that_started_it_is_killed() {
    if std::env::var_os(CHILD).is_some() {
        let sandbox = Sandbox::new();
        let (daemon, socket) = daemon(&sandbox);
        println!("{CHILD} {} {}", daemon.0.id(), socket.display());
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
    let _left = Left(rustix::process::Pid::from_raw(pid).unwrap());
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
fn a_test_cannot_run_koi_against_this_account_s_home_or_installs() {
    let account = std::env::home_dir().unwrap();
    let sandbox = Sandbox::new();
    let set: Vec<_> = process::koi(sandbox.path())
        .get_envs()
        .map(|(name, value)| (name.to_owned(), value.is_some()))
        .collect();
    for name in [
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "HOMEBREW_PREFIX",
    ] {
        assert!(
            set.contains(&(name.into(), false)),
            "{name} reaches a sandboxed koi"
        );
    }
    let run = |name: &str, value: &Path| {
        process::koi(sandbox.path())
            .args(["help", "--json"])
            .env("HOME", sandbox.path())
            .env(name, value)
            .output()
            .unwrap()
    };
    for (name, value) in [
        ("HOME", account.clone()),
        ("XDG_STATE_HOME", account.join(".local/state")),
        ("XDG_CONFIG_HOME", account.join(".config")),
        ("KOIDRIVE_TEST_INSTALL_INVENTORY", PathBuf::from("/")),
        ("HOMEBREW_PREFIX", PathBuf::from("/opt/homebrew")),
    ] {
        let output = run(name, &value);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(78), "{name}: {stderr}");
        assert!(
            stderr.starts_with("koi: a test must not"),
            "{name}: {stderr}"
        );
    }
    assert!(
        run("XDG_STATE_HOME", &sandbox.path().join("state"))
            .status
            .success()
    );
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

#[test]
fn another_test_process_leaves_a_sandbox_whole_while_it_ends() {
    let test = "another_test_process_leaves_a_sandbox_whole_while_it_ends";
    if let Some(parent) = std::env::var_os("KOI_SANDBOX_TEST_PARENT") {
        drop(Sandbox::in_dir(Path::new(&parent)));
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::in_dir(parent.path());
    let root = sandbox.path().to_path_buf();
    let lifeline = sandbox.lifeline().to_path_buf();
    let exiting = std::fs::File::open(lifeline.join("alive")).unwrap();
    rustix::fs::flock(&exiting, rustix::fs::FlockOperation::LockShared).unwrap();
    let ending = std::thread::spawn(move || drop(sandbox));
    until("the sandbox never let its koi go", || {
        std::fs::File::open(lifeline.join("held")).is_ok_and(|held| {
            rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockShared).is_ok()
        })
    });

    let swept = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("KOI_SANDBOX_TEST_PARENT", parent.path())
        .output()
        .unwrap();
    let whole = root.exists();
    drop(exiting);
    let ended = ending.join();

    assert!(
        swept.status.success(),
        "{}",
        String::from_utf8_lossy(&swept.stdout)
    );
    assert!(
        whole,
        "another test process removed a sandbox that was still ending"
    );
    assert!(ended.is_ok());
    assert!(!root.exists());
}
