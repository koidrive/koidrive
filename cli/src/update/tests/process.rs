// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_quiet_echo_holds_child_lines_until_a_failure_replays_them() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("logs/update.log");
    let mut quiet = Echo::open(&log, false, false);
    quiet.line("Signature verified for SHA256SUMS", false);
    quiet.line("SHA256 mismatch", true);
    assert_eq!(
        quiet.held,
        ["Signature verified for SHA256SUMS", "SHA256 mismatch"]
    );
    quiet.replay();
    assert!(quiet.held.is_empty());
    drop(quiet);
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "Signature verified for SHA256SUMS\nSHA256 mismatch\n"
    );
    for (json, verbose) in [(false, true), (true, false)] {
        let mut shown = Echo::open(&log, json, verbose);
        shown.line("Signature verified for SHA256SUMS", false);
        assert!(shown.held.is_empty());
    }
}

#[test]
fn the_update_log_records_the_state_folder_it_creates_and_nothing_beside_another_log() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    drop(open_log(
        &paths,
        &paths.state_dir.join("update.log"),
        true,
        false,
    ));
    assert!(koidrive_state_file::made(&paths.state_dir));

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let logs = dir.path().join("Library/Logs/Koidrive");
    drop(open_log(&paths, &logs.join("update.log"), true, false));
    assert!(logs.join("update.log").exists());
    assert!(!logs.join(koidrive_state_file::MADE).exists());
    assert!(!paths.state_dir.exists());
}

#[test]
fn reconcile_subprocess() {
    let Some(dir) = std::env::var_os("KOI_RECONCILE_TEST_DIR").map(PathBuf::from) else {
        return;
    };
    let mode = std::env::var("KOI_RECONCILE_TEST_MODE").unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let mut command = Command::new(dir.join("koi"));
        command.arg(&dir);
        let mut echo = echo(&dir);
        if mode == "cancel" {
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    echo.run(command, Duration::from_secs(10), Child::Reconcile)
                )
                .await
                .is_err()
            );
        } else {
            let within = if mode == "timeout" {
                Duration::from_millis(100)
            } else {
                Duration::from_secs(10)
            };
            let result = echo.run(command, within, Child::Reconcile).await.unwrap();
            assert!(result.is_none());
        }
    });
}

fn surviving_reconcile(mode: &str) {
    let dir = tempfile::tempdir().unwrap();
    executable(
        &dir.path().join("koi"),
        "#!/bin/sh\ntouch \"$1/ready\"\ni=0\nwhile [ ! -f \"$1/continue\" ] && [ $i -lt 250 ]; do sleep 0.02; i=$((i+1)); done\n[ -f \"$1/continue\" ] && touch \"$1/completed\"\n",
    );
    let mut parent = std::process::Command::new(std::env::current_exe().unwrap());
    parent
        .args([
            "--exact",
            "update::tests::reconcile_subprocess",
            "--nocapture",
        ])
        .env("KOI_RECONCILE_TEST_DIR", dir.path())
        .env("KOI_RECONCILE_TEST_MODE", mode)
        .process_group(0);
    let mut parent = parent.spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !dir.path().join("ready").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(dir.path().join("ready").exists());
    if mode == "sigint" {
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-INT", "--", &format!("-{}", parent.id())])
                .status()
                .unwrap()
                .success()
        );
    }
    loop {
        if let Some(status) = parent.try_wait().unwrap() {
            if mode == "sigint" {
                assert!(!status.success());
            } else {
                assert!(status.success());
            }
            break;
        }
        if std::time::Instant::now() >= deadline {
            parent.kill().unwrap();
            parent.wait().unwrap();
            panic!("foreground reconcile wait did not end");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    fs::write(dir.path().join("continue"), "").unwrap();
    while !dir.path().join("completed").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        dir.path().join("completed").exists(),
        "reconciliation must finish after {mode}"
    );
}

#[test]
fn reconciliation_survives_foreground_process_group_interrupt() {
    surviving_reconcile("sigint");
}

#[test]
fn reconciliation_survives_foreground_wait_timeout() {
    surviving_reconcile("timeout");
}

#[test]
fn reconciliation_survives_foreground_wait_cancellation() {
    surviving_reconcile("cancel");
}

#[tokio::test]
async fn cancellable_children_are_killed_when_the_wait_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new("/bin/sh");
    let pid_file = dir.path().join("pid");
    command
        .args(["-c", "echo $$ > \"$1\"; exec sleep 30", "sh"])
        .arg(&pid_file);
    let result = echo(dir.path())
        .run(command, Duration::from_millis(100), Child::Progress)
        .await
        .unwrap();
    assert!(result.is_none());
    let pid = fs::read_to_string(pid_file).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if !std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "cancellable child is still alive"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
