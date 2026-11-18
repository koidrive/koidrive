// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn the_daemon_checks_for_a_release_and_koi_status_names_it() {
    use tokio::io::AsyncBufReadExt;

    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join(".local/bin/koi"));
    let mut daemon = tokio::process::Command::from(koi_at(
        &binary,
        home.path(),
        &github,
        &["_daemon", "--json"],
    ))
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let mut events = tokio::io::BufReader::new(daemon.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(30), async {
        let started = events.next_line().await.unwrap().unwrap();
        let started: Value = serde_json::from_str(&started).unwrap();
        schema::check(&started);
        assert_eq!(started["event"], "started", "{started}");
        let mut changes = koidrive_ipc::watch(
            Path::new(started["socket"].as_str().unwrap()),
            rustix::process::getuid().as_raw(),
        )
        .await
        .unwrap()
        .unwrap();
        loop {
            let response = changes.next().await.unwrap().unwrap();
            let koidrive_ipc::Response::State { status, .. } = response else {
                panic!("{response:?}");
            };
            if matches!(
                (status.update, development_version()),
                (koidrive_driver::Update::Off, true)
                    | (koidrive_driver::Update::Available { .. }, false)
            ) {
                break;
            }
        }
    })
    .await
    .expect("the daemon starts and publishes its release check");
    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let status = envelope(&output);
    let text = koi_at(&binary, home.path(), &github, &["status"])
        .output()
        .unwrap();
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    assert!(output.status.success(), "{status:#}");
    assert_eq!(status["data"]["running"], true, "{status:#}");
    let update = &status["data"]["update"];
    if development_version() {
        assert_eq!(
            update,
            &serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
        );
        assert!(!String::from_utf8(text.stdout).unwrap().contains("Update:"));
        assert!(github.requests.lock().unwrap().is_empty());
        return;
    }
    assert_eq!(update["state"], "available", "{status:#}");
    assert_eq!(update["latest"], "99.0.0");
    assert_eq!(update["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        update["url"],
        "https://github.com/koidrive/koidrive/releases/tag/v99.0.0"
    );
    let command = format!(
        "{} update",
        std::fs::canonicalize(&binary).unwrap().display()
    );
    assert_eq!(update["command"], command);
    assert!(
        update["checked_at"].as_str().unwrap().ends_with('Z'),
        "{update}"
    );
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.contains(&format!(
            "Update: Koidrive 99.0.0 is out (this device runs {}). Run: {command}\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{text}"
    );
    let requests = github.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one check a day: {requests:?}");
    let mut headers: Vec<String> = (requests[0].lines().skip(1))
        .map(|line| line.split(':').next().unwrap().to_ascii_lowercase())
        .collect();
    headers.sort();
    assert_eq!(
        headers,
        ["accept", "host", "user-agent", "x-github-api-version"]
    );
}

#[test]
fn without_a_daemon_status_reads_the_last_check_and_the_switch_turns_it_off() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join(".local/bin/koi"));
    let state = home.path().join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("update-check.json"),
        serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "etag": "\"e1\"",
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        })
        .to_string(),
    )
    .unwrap();
    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let update = envelope(&output)["data"]["update"].clone();
    if development_version() {
        assert_eq!(
            update,
            serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
        );
    } else {
        assert_eq!(update["state"], "available");
        assert_eq!(update["checked_at"], "2025-11-13T04:00:00Z");
    }

    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .env("KOIDRIVE_UPDATE_CHECK", "off")
        .output()
        .unwrap();
    let update = envelope(&output)["data"]["update"].clone();
    assert_eq!(
        update,
        serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
    );
    assert!(
        github.requests.lock().unwrap().is_empty(),
        "status asks nothing"
    );
}

#[test]
fn cached_status_names_brew_upgrade_for_a_formula_and_koi_update_for_a_cask() {
    for cask in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let github = github("v99.0.0");
        let binary = homebrew_binary(home.path(), cask);
        let script = home.path().join(".local/bin/koi");
        executable(
            &script,
            "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 100.0.0'\n",
        );
        let alias = home.path().join("status-alias");
        std::os::unix::fs::symlink(&binary, &alias).unwrap();
        let state = home.path().join("state/koidrive");
        std::fs::create_dir_all(&state).unwrap();
        let cached = serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "etag": "\"e1\"",
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        })
        .to_string();
        std::fs::write(state.join("update-check.json"), &cached).unwrap();
        let output = koi_at(&alias, home.path(), &github, &["status", "--json"])
            .env("PATH", home.path().join("brew/bin"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let update = envelope(&output)["data"]["update"].clone();
        let output = koi_at(&alias, home.path(), &github, &["status"])
            .env("PATH", home.path().join("brew/bin"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        if development_version() {
            assert_eq!(
                update,
                serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
            );
            assert!(!text.contains("Update:"), "{text}");
        } else {
            let command = if cask {
                format!(
                    "{} update",
                    std::fs::canonicalize(&binary).unwrap().display()
                )
            } else {
                "brew upgrade koidrive".to_owned()
            };
            assert_eq!(
                update,
                serde_json::json!({
                    "state": "available", "version": env!("CARGO_PKG_VERSION"),
                    "latest": "99.0.0", "url": "https://example.com/v99.0.0",
                    "checked_at": "2025-11-13T04:00:00Z", "checked_at_ms": 1763006400000_i64,
                    "command": command,
                })
            );
            assert!(
                text.contains(&format!(
                    "Update: Koidrive 99.0.0 is out (this device runs {}). Run: {command}\n",
                    env!("CARGO_PKG_VERSION")
                )),
                "{text}"
            );
            assert!(!text.contains("Run: koi update"), "{text}");
        }
        assert!(github.requests.lock().unwrap().is_empty());
        assert!(!script.with_extension("calls").exists());
        assert!(!home.path().join("brew/bin/brew.calls").exists());
        assert_eq!(
            std::fs::read_to_string(state.join("update-check.json")).unwrap(),
            cached
        );
    }
}

#[test]
fn an_installer_notice_names_its_real_binary_despite_another_koi_on_path() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join("standalone copy's/bin/koi"));
    let alias = home.path().join("installer-alias");
    std::os::unix::fs::symlink(&binary, &alias).unwrap();
    let bin = home.path().join("path/bin");
    let other = bin.join("koi");
    executable(
        &other,
        "#!/bin/sh\necho selected >> \"$0.calls\"\nexit 99\n",
    );
    let state = home.path().join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("update-check.json"),
        serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        })
        .to_string(),
    )
    .unwrap();
    let status = koi_at(&alias, home.path(), &github, &["status", "--json"])
        .env("PATH", &bin)
        .output()
        .unwrap();
    let update = envelope(&status)["data"]["update"].clone();
    if development_version() {
        assert_eq!(update["state"], "off");
        assert!(update.get("command").is_none());
        return;
    }
    let resolved = std::fs::canonicalize(&binary).unwrap();
    let expected = format!(
        "'{}' update",
        resolved.to_str().unwrap().replace('\'', "'\\''")
    );
    assert_eq!(update["command"], expected);
    let status = koi_at(&alias, home.path(), &github, &["status"])
        .env("PATH", &bin)
        .output()
        .unwrap();
    assert!(
        String::from_utf8(status.stdout)
            .unwrap()
            .contains(&format!("Run: {expected}\n"))
    );
    let output = koi_at(
        Path::new("/bin/sh"),
        home.path(),
        &github,
        &["-c", &expected],
    )
    .env("PATH", &bin)
    .env("KOIDRIVE_INSTALL_SCRIPT_URL", "not a URL")
    .output()
    .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("not a URL")
    );
    assert!(!other.with_extension("calls").exists());
    assert_eq!(std::fs::read_link(alias).unwrap(), binary);
}
