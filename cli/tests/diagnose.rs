// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use koidrive_test_support::Sandbox;

#[path = "support/process.rs"]
mod process;

const SERVER: &str = "https://koi-ada-7f3k.workers.dev";
const TOKEN: &str = "kdr_9f8e7d6c5b4a39281706f5e4d3c2b1a0";

fn koi(home: &Path) -> Command {
    let mut command = process::koi(home);
    command
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env_remove("KOIDRIVE_LOG")
        .env("KOIDRIVE_UPDATE_CHECK", "0");
    command
}

fn paired_home() -> Sandbox {
    let home = Sandbox::new();
    let state = home.path().join(".local/state/koidrive");
    fs::create_dir_all(&state).unwrap();
    let credential = state.join("credential.json");
    let text = serde_json::json!({
        "server": SERVER,
        "user_id": "u1",
        "user_name": "Ada Lovelace",
        "device_id": "d1",
        "device_name": "Ada's MacBook",
        "refresh_credential": TOKEN,
    });
    fs::write(&credential, text.to_string()).unwrap();
    fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
    home
}

fn agent_log(home: &Path) -> PathBuf {
    let paths = koidrive_driver::Paths::from_vars(|name| (name == "HOME").then(|| home.into()));
    let label = format!("dev.koidrive.koi.{}", paths.unwrap().state_key());
    home.join(format!("Library/Logs/Koidrive/{label}.log"))
}

#[test]
fn diagnose_writes_a_file_without_the_profiles_paths_names_server_or_token() {
    let home = paired_home();
    if cfg!(target_os = "macos") {
        let log = agent_log(home.path());
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        let folder = home.path().join("Secret Plans");
        let line = format!(
            "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=cannot read {}/budget.xlsx at {SERVER}/v1 with {TOKEN}: Permission denied (os error 13)\n",
            folder.display()
        );
        fs::write(&log, line).unwrap();
        fs::write(
            format!("{}.1", log.display()),
            "2026-11-12T06:29:22.000001Z  INFO koidrive_cli: the daemon is running\n",
        )
        .unwrap();
    }
    let cwd = tempfile::tempdir().unwrap();
    let output = koi(home.path())
        .arg("diagnose")
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = String::from_utf8(output.stdout).unwrap();
    let text = fs::read_to_string(path.trim_end()).unwrap();
    let cwd = fs::canonicalize(cwd.path()).unwrap();
    assert!(path.starts_with(&cwd.display().to_string()), "{path}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("github.com/koidrive/koidrive/issues")
    );
    let home = home.path().display().to_string();
    for planted in [
        &home,
        "Ada",
        "koi-ada-7f3k",
        "workers.dev",
        TOKEN,
        "Secret",
        "budget",
    ] {
        assert!(!text.contains(planted), "{planted} in:\n{text}");
    }
    assert!(text.contains("\"device_name\": \"<device-1>\""), "{text}");
    if cfg!(target_os = "macos") {
        assert!(text.contains("sync of a folder halted"), "{text}");
        assert!(text.contains(": Permission denied (os error 13)"), "{text}");
        assert!(text.contains("the daemon is running"), "{text}");
    }
}

#[test]
fn each_run_writes_its_own_file_that_only_its_owner_can_read() {
    let home = paired_home();
    let cwd = tempfile::tempdir().unwrap();
    let paths: Vec<String> = (0..3)
        .map(|_| {
            let output = koi(home.path())
                .arg("diagnose")
                .current_dir(cwd.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .unwrap()
                .trim_end()
                .to_owned()
        })
        .collect();
    let mut unique = paths.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 3, "{paths:?}");
    for path in paths {
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{path}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn the_daemon_moves_a_full_launch_agent_log_aside() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let home = Sandbox::new();
    let log = agent_log(home.path());
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    let full = fs::File::create(&log).unwrap();
    full.set_len(11 * 1024 * 1024).unwrap();
    let stderr = fs::OpenOptions::new().append(true).open(&log).unwrap();
    let mut daemon = koi(home.path())
        .arg("_daemon")
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let started = loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if text.contains("the daemon is running") || Instant::now() > deadline {
            break text;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(started.contains("the daemon is running"), "{started}");
    assert!(fs::metadata(&log).unwrap().len() < 1024 * 1024);
    let old = PathBuf::from(format!("{}.1", log.display()));
    assert_eq!(fs::metadata(old).unwrap().len(), 11 * 1024 * 1024);
}
