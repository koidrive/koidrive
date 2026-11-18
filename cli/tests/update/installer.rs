// SPDX-License-Identifier: Apache-2.0

#[test]
fn development_update_never_contacts_github_or_runs_an_installer() {
    if !development_version() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    homebrew(home.path(), "0.0.1");
    let script = home.path().join(".local/bin/koi");
    let app = home
        .path()
        .join("Applications/Koidrive.app/Contents/Helpers/koi");
    for path in [&script, &app] {
        executable(
            path,
            "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 0.0.1'\n",
        );
    }
    for switch in ["on", "off"] {
        let output = koi(home.path(), &github, &["update", "--json"])
            .env("KOIDRIVE_UPDATE_CHECK", switch)
            .output()
            .unwrap();
        assert!(development_update(&output, home.path(), &github));
        assert!(!script.with_extension("calls").exists());
        assert!(!app.with_extension("calls").exists());
    }
    let output = koi(home.path(), &github, &["update"])
        .env("KOIDRIVE_UPDATE_CHECK_URL", "not a URL")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Development builds do not update to releases.\n"
    );
    assert!(github.requests.lock().unwrap().is_empty());
}

#[test]
fn koi_update_installs_a_release_and_hands_the_service_to_the_new_koi() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
    let calls = homebrew(home.path(), "100.0.0");
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(
        envelope["data"],
        serde_json::json!({
            "from": env!("CARGO_PKG_VERSION"),
            "to": "99.0.0",
            "via": "installer",
            "result": "updated",
            "service": { "manager": "launchd", "name": "dev.koidrive.koi", "state": "running" },
        })
    );
    assert_eq!(
        std::fs::read_to_string(binary.parent().unwrap().join("installer.calls")).unwrap(),
        format!(
            "{}|v99.0.0\n",
            std::fs::canonicalize(binary.parent().unwrap())
                .unwrap()
                .display()
        )
    );
    assert!(!calls.exists());
    assert_eq!(installer.requests.lock().unwrap().len(), 1);
    assert!(
        std::fs::read_to_string(binary.with_extension("calls"))
            .unwrap()
            .contains("_reconcile --json\n")
    );
    let requests = github.requests.lock().unwrap();
    assert!(
        requests[0].starts_with("GET /repos/koidrive/koidrive/releases?per_page=20 HTTP/1.1\r\n"),
        "{requests:?}"
    );
}

#[test]
fn koi_update_reconciles_a_concurrently_installed_newest_release_without_an_installer() {
    let home = tempfile::tempdir().unwrap();
    let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
    let replacement = binary.clone();
    let releases = serde_json::json!([{
        "tag_name": "v99.0.0",
        "html_url": "https://example.com/v99.0.0",
        "assets": [{ "name": asset() }, { "name": "SHA256SUMS" }],
    }]);
    let github = http_before_reply(releases.to_string(), move || {
        let staged = replacement.with_extension("new");
        executable(
            &staged,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi 99.0.0' ;;\n_reconcile) echo '{REPORT}' ;;\nesac\n"
            ),
        );
        std::fs::rename(staged, &replacement).unwrap();
    });
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    assert_eq!(
        envelope["data"],
        serde_json::json!({
            "from": "99.0.0", "to": "99.0.0", "via": "installer", "result": "unchanged",
            "service": { "manager": "launchd", "name": "dev.koidrive.koi", "state": "running" },
        })
    );
    assert!(installer.requests.lock().unwrap().is_empty());
    assert!(
        std::fs::read_to_string(binary.with_extension("calls"))
            .unwrap()
            .contains("_reconcile --json\n")
    );
}

#[test]
fn koi_update_rechecks_foreign_and_legacy_caches_but_keeps_same_target_conditionals() {
    for cached_asset in [
        Some("koidrive-foreign-target.tar.gz".to_owned()),
        None,
        Some(asset()),
    ] {
        let home = tempfile::tempdir().unwrap();
        let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
        let state = home.path().join("state/koidrive");
        std::fs::create_dir_all(&state).unwrap();
        let mut cached = serde_json::json!({
            "checked_at": 1763006400000_i64,
            "successful_at": 1763006400000_i64,
            "etag": "\"cached\"",
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        });
        if let Some(asset) = &cached_asset {
            cached["asset"] = asset.clone().into();
        }
        std::fs::write(state.join("update-check.json"), cached.to_string()).unwrap();
        if development_version() {
            let github = github("v99.0.0");
            let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
                .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
                .output()
                .unwrap();
            assert!(development_update(&output, home.path(), &github));
            continue;
        }
        let same_target = cached_asset.as_deref() == Some(asset().as_str());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let github = GitHub {
            base: format!("http://{}", listener.local_addr().unwrap()),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            let body = serde_json::json!([{
                "tag_name": "v99.0.0",
                "html_url": "https://example.com/v99.0.0",
                "assets": [{ "name": "koidrive-foreign-target.tar.gz" }],
            }])
            .to_string();
            let response = if same_target {
                "HTTP/1.1 304 Not Modified\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    .to_owned()
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            stream.write_all(response.as_bytes()).unwrap();
            request
        });
        let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
            .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
            .output()
            .unwrap();
        let request = server.join().unwrap().to_ascii_lowercase();
        assert_eq!(
            request.contains("if-none-match: \"cached\""),
            same_target,
            "{request}"
        );
        let envelope = envelope(&output);
        assert_eq!(output.status.success(), same_target, "{envelope:#}");
        if same_target {
            assert_eq!(envelope["outcome"], "ok");
            assert_eq!(envelope["data"]["to"], "99.0.0");
            assert_eq!(envelope["data"]["result"], "updated");
        } else {
            assert_eq!(envelope["outcome"], "error");
            assert_eq!(
                envelope["error"]["message"],
                "GitHub lists no Koidrive release for this system"
            );
        }
        assert_eq!(
            installer.requests.lock().unwrap().len(),
            usize::from(same_target)
        );
    }
}

#[test]
fn koi_update_uses_githubs_answer_from_the_last_day_when_github_refuses_the_list() {
    let hour = 3_600_000_i64;
    for (age, updates) in [(hour, true), (25 * hour, false)] {
        let home = tempfile::tempdir().unwrap();
        let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
        let state = home.path().join("state/koidrive");
        std::fs::create_dir_all(&state).unwrap();
        let now = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH))
            .unwrap()
            .as_millis() as i64;
        let cached = serde_json::json!({
            "asset": asset(),
            "checked_at": now - age,
            "successful_at": now - age,
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        });
        std::fs::write(state.join("update-check.json"), cached.to_string()).unwrap();
        let github = serve(
            "HTTP/1.1 403 Forbidden\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset: 4102444800\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned(),
            || {},
        );
        let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
            .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
            .output()
            .unwrap();
        if development_update(&output, home.path(), &github) {
            return;
        }
        let envelope = envelope(&output);
        assert_eq!(output.status.success(), updates, "{age}: {envelope:#}");
        match updates {
            true => {
                assert_eq!(envelope["data"]["to"], "99.0.0", "{envelope:#}");
                assert_eq!(envelope["data"]["result"], "updated", "{envelope:#}");
            }
            false => assert_eq!(
                envelope["error"]["message"],
                "GitHub did not send the list of Koidrive releases; try again later",
                "{envelope:#}"
            ),
        }
        assert_eq!(github.requests.lock().unwrap().len(), 1);
        assert_eq!(
            installer.requests.lock().unwrap().len(),
            usize::from(updates)
        );
    }
}

#[test]
fn koi_update_preserves_disabled_login_items_and_only_fails_for_a_real_problem() {
    for problem in [None, Some("the service could not be registered")] {
        let home = tempfile::tempdir().unwrap();
        let github = github("v99.0.0");
        let mut report: Value = serde_json::from_str(REPORT).unwrap();
        report["data"]["service"]["state"] = Value::from("switched_off");
        if let Some(problem) = problem {
            report["data"]["service"]["problem"] = Value::from(problem);
        }
        let (binary, installer) = script_install(home.path(), "99.0.0", &report.to_string());
        let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
            .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
            .output()
            .unwrap();
        if development_update(&output, home.path(), &github) {
            continue;
        }
        let envelope = envelope(&output);
        assert_eq!(output.status.success(), problem.is_none(), "{envelope:#}");
        assert_eq!(envelope["data"]["result"], "updated");
        assert_eq!(envelope["data"]["service"]["state"], "switched_off");
        copy_koi(&binary);
        let output = koi_at(&binary, home.path(), &github, &["update"])
            .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
            .output()
            .unwrap();
        assert_eq!(output.status.success(), problem.is_none());
        let text = String::from_utf8(output.stdout).unwrap();
        match problem {
            None => assert!(text.contains("Login Items"), "{text}"),
            Some(problem) => assert!(text.contains(problem), "{text}"),
        }
    }
}

#[test]
fn schema_one_status_without_an_update_notice_is_still_valid() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let output = koi(home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let mut envelope = envelope(&output);
    assert!(
        envelope["data"]
            .as_object_mut()
            .unwrap()
            .remove("update")
            .is_some()
    );
    schema::check(&envelope);
}

#[test]
fn koi_update_text_hides_the_reconcile_envelope_but_keeps_it_in_the_log() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
    let output = koi_at(&binary, home.path(), &github, &["update"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    if development_version() {
        assert_eq!(text, "Development builds do not update to releases.\n");
        assert!(github.requests.lock().unwrap().is_empty());
        assert!(!home.path().join("brew/bin/brew.calls").exists());
        assert!(!home.path().join("brew/opt/koidrive/bin/koi.calls").exists());
        return;
    }
    assert!(
        text.contains(&format!(
            "Updated Koidrive from {} to 99.0.0.",
            env!("CARGO_PKG_VERSION")
        )),
        "{text}"
    );
    assert!(!text.contains("schema_version"), "{text}");
    let log = update_log(home.path());
    assert!(std::fs::read_to_string(log).unwrap().contains(REPORT));
}

#[test]
fn koi_update_shows_its_steps_and_keeps_the_installer_lines_in_the_log() {
    let Some((output, log)) = update_text(CHECKS, &["update"], REPORT) else {
        return;
    };
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stdout}{stderr}");
    assert_eq!(
        stderr,
        "Downloading Koidrive 99.0.0\nDownloaded, checked and installed Koidrive 99.0.0.\nChecking the background service\nChecked the background service.\n"
    );
    assert!(
        stdout.starts_with(&format!(
            "Updated Koidrive from {} to 99.0.0.\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("verified"), "{stdout}");
    assert!(log.contains("Signature verified for SHA256SUMS"), "{log}");
}

#[test]
fn koi_update_verbose_prints_the_installer_lines() {
    let Some((output, _)) = update_text(CHECKS, &["update", "--verbose"], REPORT) else {
        return;
    };
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.starts_with(
            "Signature verified for SHA256SUMS\nSHA256 verified for koidrive.tar.gz\n"
        ),
        "{stdout}"
    );
}

#[test]
fn a_failed_install_shows_what_the_installer_said() {
    let said = "echo 'Downloaded koidrive.tar.gz'\necho 'SHA256 mismatch for koidrive.tar.gz' >&2\nexit 1\n";
    let Some((output, _)) = update_text(said, &["update"], REPORT) else {
        return;
    };
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.starts_with("Downloading Koidrive 99.0.0\n"),
        "{stderr}"
    );
    for said in [
        "Downloaded koidrive.tar.gz\n",
        "SHA256 mismatch for koidrive.tar.gz\n",
    ] {
        assert!(stderr.contains(said), "{stderr}");
    }
    assert!(
        stderr.contains("install.sh failed (exit status 1)"),
        "{stderr}"
    );
    assert!(!stderr.contains("Downloaded, checked"), "{stderr}");
}

#[test]
fn koi_update_on_a_terminal_shows_setup_style_steps_without_the_checks() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let github = github("v99.0.0");
    let (binary, installer) = script_install_saying(home.path(), "99.0.0", REPORT, CHECKS);
    let mut command = koi_at(&binary, home.path(), &github, &["update"]);
    command
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .env("NO_COLOR", "1")
        .env("TERM", "xterm-256color")
        .env_remove("KOI_PLAIN");
    let terminal = on_a_terminal(command, None);
    if development_version() {
        assert!(terminal.contains("Development builds do not update to releases."));
        return;
    }
    assert!(
        terminal.contains("◇  Downloaded, checked and installed Koidrive 99.0.0\n"),
        "{terminal}"
    );
    assert!(
        terminal.contains("◇  Checked the background service\n"),
        "{terminal}"
    );
    assert!(
        terminal.contains(&format!(
            "Updated Koidrive from {} to 99.0.0.",
            env!("CARGO_PKG_VERSION")
        )),
        "{terminal}"
    );
    assert!(!terminal.contains("verified"), "{terminal}");
}

#[test]
fn a_failed_service_check_shows_only_its_own_output() {
    let Some((output, _)) = update_text(CHECKS, &["update"], STOPPED) else {
        return;
    };
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.starts_with("Downloading Koidrive 99.0.0\nDownloaded, checked and installed Koidrive 99.0.0.\nChecking the background service\n"),
        "{stderr}"
    );
    assert!(!stderr.contains("verified"), "{stderr}");
}

#[test]
fn a_failed_app_report_fails_the_service_check() {
    let Some((output, _)) = update_text(CHECKS, &["update"], APP_FAILED) else {
        return;
    };
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("Koidrive.app did not start"), "{stderr}");
    assert!(
        !stderr.contains("Checked the background service"),
        "{stderr}"
    );
}

#[test]
fn an_installer_that_leaves_the_old_koi_fails_its_step() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join(".local/bin/koi"));
    let installer = http("echo 'Installed koi'\nexit 0\n".to_owned());
    let output = koi_at(&binary, home.path(), &github, &["update"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_version() {
        assert!(output.status.success());
        return;
    }
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.starts_with("Downloading Koidrive 99.0.0\nInstalled koi\n"),
        "{stderr}"
    );
    assert!(stderr.contains("is still Koidrive"), "{stderr}");
    assert!(!stderr.contains("Downloaded, checked"), "{stderr}");
    assert!(
        !stderr.contains("Checking the background service"),
        "{stderr}"
    );
}

#[test]
fn a_homebrew_password_prompt_stays_on_screen_until_it_is_answered() {
    if development_version() {
        return;
    }
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    let typed = home.path().join("typed");
    scripted_brew(
        &prefix,
        0,
        &format!(
            "  printf 'Password:' > /dev/tty\n  IFS= read -r answer < /dev/tty\n  printf '%s' \"$answer\" > '{}'\n{}",
            typed.display(),
            replacing(&koi, "99.0.0", REPORT)
        ),
    );
    let mut command = koi_at(&link, home.path(), &github, &["update"]);
    command
        .env("PATH", home.path().join("empty"))
        .env("NO_COLOR", "1")
        .env("TERM", "xterm-256color")
        .env_remove("KOI_PLAIN");
    let terminal = on_a_terminal(command, Some((Duration::from_secs(3), b"secret\n")));
    assert_eq!(std::fs::read_to_string(&typed).unwrap(), "secret");
    let (_, after) = terminal.split_once("Password:").unwrap();
    let waiting = after
        .split_once("secret")
        .map_or(after, |(before, _)| before);
    assert!(
        !waiting.contains('\r') && !waiting.contains('\u{1b}'),
        "{waiting:?}"
    );
}

#[test]
fn koi_update_rejects_a_successful_installer_that_leaves_the_old_binary_installed() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join(".local/bin/koi"));
    let installer = http("exit 0\n".to_owned());
    let started = Instant::now();
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "error");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains(env!("CARGO_PKG_VERSION"))
    );
    assert_ne!(envelope["data"]["result"], "updated");
    assert!(!binary.with_extension("calls").exists());
    assert_eq!(installer.requests.lock().unwrap().len(), 1);
}

#[test]
fn koi_update_reports_the_version_actually_installed_when_it_is_above_the_target() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (binary, installer) = script_install(home.path(), "100.0.0", REPORT);
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["data"]["to"], "100.0.0");
    assert_eq!(envelope["data"]["result"], "updated");
}

#[test]
fn koi_update_through_a_script_alias_installs_into_the_canonical_directory() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (original, installer) = script_install(home.path(), "99.0.0", REPORT);
    let directory = home.path().join("standalone install/bin");
    std::fs::create_dir_all(directory.parent().unwrap()).unwrap();
    std::fs::rename(original.parent().unwrap(), &directory).unwrap();
    let binary = directory.join("koi");
    let first = home.path().join("first-alias");
    let alias = home.path().join("second-alias");
    std::os::unix::fs::symlink(&binary, &first).unwrap();
    std::os::unix::fs::symlink(&first, &alias).unwrap();
    let calls = homebrew(home.path(), "100.0.0");
    let output = koi_at(&alias, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["data"]["via"], "installer");
    assert_eq!(envelope["data"]["result"], "updated");
    assert_eq!(envelope["data"]["to"], "99.0.0");
    assert_eq!(
        std::fs::read_to_string(directory.join("installer.calls")).unwrap(),
        format!(
            "{}|v99.0.0\n",
            std::fs::canonicalize(&directory).unwrap().display()
        )
    );
    assert!(
        std::fs::read_to_string(binary.with_extension("calls"))
            .unwrap()
            .contains("_reconcile --json\n")
    );
    assert_eq!(std::fs::read_link(&alias).unwrap(), first);
    assert_eq!(std::fs::read_link(&first).unwrap(), binary);
    assert!(!calls.exists());
    assert!(!home.path().join("koi").exists());
    assert!(!original.exists());
}
