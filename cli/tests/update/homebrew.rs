// SPDX-License-Identifier: Apache-2.0

#[test]
fn koi_update_upgrades_a_formula_with_the_brew_that_owns_it() {
    for alias in [false, true] {
        let home = Sandbox::new();
        let github = github("v99.0.0");
        let link = homebrew_binary(home.path(), false);
        let koi = std::fs::canonicalize(&link).unwrap();
        let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
        let calls = upgrading_brew(&prefix, &koi, "99.0.0", 0);
        let elsewhere = home.path().join("elsewhere/bin");
        executable(
            &elsewhere.join("brew"),
            "#!/bin/sh\necho selected >> \"$0.calls\"\nexit 99\n",
        );
        let invoked = if alias {
            let first = home.path().join("alias-one");
            let second = home.path().join("alias-two");
            std::os::unix::fs::symlink(&link, &first).unwrap();
            std::os::unix::fs::symlink(first, &second).unwrap();
            second
        } else {
            link.clone()
        };
        let output = koi_at(&invoked, home.path(), &github, &["update", "--json"])
            .env("PATH", &elsewhere)
            .output()
            .unwrap();
        if development_update(&output, home.path(), &github) {
            continue;
        }
        let envelope = envelope(&output);
        assert!(output.status.success(), "{envelope:#}");
        assert_eq!(
            envelope["data"],
            serde_json::json!({
                "from": env!("CARGO_PKG_VERSION"),
                "to": "99.0.0",
                "via": "homebrew",
                "result": "updated",
                "service": { "manager": "launchd", "name": "dev.koidrive.koi", "state": "running" },
            })
        );
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap(),
            "--repository koidrive/tap|1\nupgrade koidrive|1\n"
        );
        assert_eq!(
            std::fs::read_to_string(prefix.join("bin/git.calls")).unwrap(),
            format!(
                "-C {} pull --ff-only --quiet\n",
                prefix.join("Library/Taps/koidrive/homebrew-tap").display()
            )
        );
        assert!(
            std::fs::read_to_string(koi.with_extension("calls"))
                .unwrap()
                .contains("_reconcile --json\n")
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("==> Upgrading koidrive\n"));
        assert!(!elsewhere.join("brew.calls").exists());
        assert_eq!(github.requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn koi_update_updates_a_cask_with_the_app_updater_and_names_brew_only_as_the_fallback() {
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), true);
    let koi = std::fs::canonicalize(&link).unwrap();
    let brew = std::fs::canonicalize(home.path().join("brew"))
        .unwrap()
        .join("bin/brew");
    let before = std::fs::read(&koi).unwrap();
    let output = koi_at(&link, home.path(), &github, &["update", "--json"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(
        message.ends_with(&format!(
            "To update, run: {} upgrade --cask koidrive",
            brew.display()
        )),
        "{message}"
    );
    assert_eq!(
        envelope["next_commands"],
        serde_json::json!([{ "argv": [brew, "upgrade", "--cask", "koidrive"] }])
    );
    assert!(!brew.with_extension("calls").exists());
    assert_eq!(std::fs::read(&koi).unwrap(), before);
    let log = std::fs::read_to_string(update_log(home.path())).unwrap();
    assert!(log.contains(message), "{log}");
}

#[cfg(target_os = "macos")]
#[test]
fn koi_update_leaves_a_cask_in_a_folder_this_user_cannot_write_to_homebrew() {
    use std::os::unix::fs::PermissionsExt;
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), true);
    let koi = std::fs::canonicalize(&link).unwrap();
    let brew = std::fs::canonicalize(home.path().join("brew"))
        .unwrap()
        .join("bin/brew");
    let applications = std::fs::canonicalize(home.path().join("Applications")).unwrap();
    let before = std::fs::read(&koi).unwrap();
    std::fs::set_permissions(&applications, std::fs::Permissions::from_mode(0o555)).unwrap();
    let output = koi_at(&link, home.path(), &github, &["update", "--json"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    std::fs::set_permissions(&applications, std::fs::Permissions::from_mode(0o755)).unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert_eq!(
        message,
        format!(
            "This user cannot write {}, so Koidrive cannot replace the Homebrew app there.\nTo update, run: {} upgrade --cask koidrive",
            applications.display(),
            brew.display()
        )
    );
    assert_eq!(
        envelope["next_commands"],
        serde_json::json!([{ "argv": [brew, "upgrade", "--cask", "koidrive"] }])
    );
    assert!(!brew.with_extension("calls").exists());
    assert_eq!(std::fs::read(&koi).unwrap(), before);
    assert_eq!(
        std::fs::read_dir(&applications).unwrap().count(),
        1,
        "the update left no second app beside the cask"
    );
}

#[test]
fn koi_update_through_homebrew_says_when_koidrive_is_already_the_newest() {
    for json in [true, false] {
        let home = Sandbox::new();
        let link = homebrew_binary(home.path(), false);
        let koi = std::fs::canonicalize(&link).unwrap();
        let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
        let calls = upgrading_brew(&prefix, &koi, env!("CARGO_PKG_VERSION"), 0);
        let tag = concat!("v", env!("CARGO_PKG_VERSION"));
        let releases = serde_json::json!([{
            "tag_name": tag,
            "html_url": format!("https://example.com/{tag}"),
            "assets": [{ "name": asset() }, { "name": "SHA256SUMS" }],
        }]);
        let replacement = koi.clone();
        let github = http_before_reply(releases.to_string(), move || {
            let staged = replacement.with_extension("new");
            executable(
                &staged,
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi {}' ;;\n_reconcile) echo '{REPORT}' ;;\nesac\n",
                    env!("CARGO_PKG_VERSION")
                ),
            );
            std::fs::rename(staged, &replacement).unwrap();
        });
        let args: &[&str] = if json {
            &["update", "--json"]
        } else {
            &["update"]
        };
        let output = koi_at(&link, home.path(), &github, args)
            .env("PATH", home.path().join("empty"))
            .output()
            .unwrap();
        if json {
            if development_update(&output, home.path(), &github) {
                return;
            }
            let envelope = envelope(&output);
            assert!(output.status.success(), "{envelope:#}");
            assert_eq!(envelope["outcome"], "unchanged");
            assert_eq!(envelope["data"]["result"], "unchanged");
            assert!(!calls.exists());
            assert!(
                std::fs::read_to_string(koi.with_extension("calls"))
                    .unwrap()
                    .contains("_reconcile --json\n")
            );
            continue;
        }
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.starts_with(&format!(
                "Koidrive {} is the newest release.\n",
                env!("CARGO_PKG_VERSION")
            )),
            "{text}"
        );
        assert!(!calls.exists());
    }
}

#[test]
fn koi_update_through_homebrew_shows_steps_and_brew_output_only_on_failure() {
    for status in [0, 1] {
        let home = Sandbox::new();
        let github = github("v99.0.0");
        let link = homebrew_binary(home.path(), false);
        let koi = std::fs::canonicalize(&link).unwrap();
        let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
        upgrading_brew(&prefix, &koi, "99.0.0", status);
        let output = koi_at(&link, home.path(), &github, &["update"])
            .env("PATH", home.path().join("empty"))
            .output()
            .unwrap();
        if development_version() {
            assert!(output.status.success());
            continue;
        }
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        if status == 0 {
            assert!(output.status.success(), "{stdout}{stderr}");
            assert_eq!(
                stderr,
                "Updating Koidrive with Homebrew\nHomebrew installed Koidrive 99.0.0.\nChecking the background service\nChecked the background service.\n"
            );
            assert!(!stdout.contains("==>"), "{stdout}");
        } else {
            assert!(!output.status.success(), "{stdout}{stderr}");
            assert!(
                stderr.starts_with("Updating Koidrive with Homebrew\n"),
                "{stderr}"
            );
            for said in [
                "==> Upgrading koidrive\n",
                "Error: koidrive did not download\n",
            ] {
                assert!(stderr.contains(said), "{stderr}");
            }
            assert!(
                stderr.contains("brew upgrade failed (exit status 1)"),
                "{stderr}"
            );
        }
    }
}

#[test]
fn an_incomplete_homebrew_update_shows_why_the_tap_did_not_refresh() {
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    scripted_brew(&prefix, 1, "  exit 0");
    let output = koi_at(&link, home.path(), &github, &["update"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    if development_version() {
        assert!(output.status.success());
        return;
    }
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.starts_with("Updating Koidrive with Homebrew\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains("fatal: unable to access https://github.com/koidrive/homebrew-tap/: Could not resolve host: github.com\n"),
        "{stderr}"
    );
    assert!(!stderr.contains("Homebrew installed"), "{stderr}");
}

#[test]
fn koi_update_names_the_brew_command_when_homebrew_fails_or_is_missing() {
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    upgrading_brew(&prefix, &koi, "99.0.0", 1);
    let before = std::fs::read(&koi).unwrap();
    let brew = prefix.join("bin/brew");
    let command = format!("{} upgrade koidrive", brew.display());
    let output = koi_at(&link, home.path(), &github, &["update", "--json"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    assert_eq!(
        envelope["error"]["message"],
        format!("brew upgrade failed (exit status 1). To update, run: {command}")
    );
    assert_eq!(
        envelope["next_commands"],
        serde_json::json!([{ "argv": [brew, "upgrade", "koidrive"] }])
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("Error: koidrive did not download\n"));
    assert_eq!(std::fs::read(&koi).unwrap(), before);
    std::fs::remove_file(&brew).unwrap();
    let output = koi_at(&link, home.path(), &github, &["update"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!(
            "Homebrew is not at {}. To update, run: {command}",
            brew.display()
        )),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&koi).unwrap(), before);
}

#[test]
fn koi_update_through_homebrew_is_incomplete_when_the_tap_lags_or_cannot_refresh() {
    for git_status in [0, 1] {
        for json in [true, false] {
            let home = Sandbox::new();
            let github = github("v99.0.0");
            let link = homebrew_binary(home.path(), false);
            let koi = std::fs::canonicalize(&link).unwrap();
            let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
            let calls = scripted_brew(
                &prefix,
                git_status,
                &format!(
                    "{}\n  echo 'Warning: koidrive/tap/koidrive {} already installed' >&2\n  exit 0",
                    replacing(&koi, env!("CARGO_PKG_VERSION"), REPORT),
                    env!("CARGO_PKG_VERSION")
                ),
            );
            let args: &[&str] = if json {
                &["update", "--json"]
            } else {
                &["update"]
            };
            let output = koi_at(&link, home.path(), &github, args)
                .env("PATH", home.path().join("empty"))
                .env("HOMEBREW_ASK", "1")
                .env("GIT_DIR", home.path().join("another-repository"))
                .output()
                .unwrap();
            if development_version() {
                assert!(output.status.success(), "{output:?}");
                return;
            }
            assert!(!output.status.success(), "{output:?}");
            let tap = prefix.join("Library/Taps/koidrive/homebrew-tap");
            let why = match git_status {
                0 => "The koidrive/tap tap does not have Koidrive 99.0.0 yet".to_owned(),
                _ => format!(
                    "Could not refresh the koidrive/tap tap at {} (git exit status 1)",
                    tap.display()
                ),
            };
            let message = format!(
                "Koidrive update is incomplete: the newest release is 99.0.0, and Homebrew has {}. {why}. To update, run: {} upgrade koidrive",
                env!("CARGO_PKG_VERSION"),
                prefix.join("bin/brew").display()
            );
            let stdout = String::from_utf8(output.stdout.clone()).unwrap();
            assert!(!stdout.contains("is the newest release"), "{stdout}");
            if json {
                let envelope = envelope(&output);
                assert_eq!(envelope["outcome"], "error");
                assert_eq!(envelope["error"]["message"], message);
            } else {
                let stderr = String::from_utf8(output.stderr).unwrap();
                assert!(stderr.contains(&message), "{stderr}");
                assert!(
                    stdout.starts_with("Koidrive update is incomplete"),
                    "{stdout}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(&calls).unwrap(),
                "--repository koidrive/tap|1\nupgrade koidrive|\n"
            );
            assert_eq!(
                std::fs::read_to_string(prefix.join("bin/git.calls")).unwrap(),
                format!("-C {} pull --ff-only --quiet|0|\n", tap.display())
            );
        }
    }
}

#[test]
fn koi_update_through_homebrew_is_incomplete_when_the_tap_lags_and_the_service_stopped() {
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    scripted_brew(
        &prefix,
        0,
        &format!(
            "{}\n  exit 0",
            replacing(
                &koi,
                env!("CARGO_PKG_VERSION"),
                &REPORT.replace("running", "stopped")
            )
        ),
    );
    let output = koi_at(&link, home.path(), &github, &["update", "--json"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    assert_eq!(
        envelope["error"]["message"],
        format!(
            "Koidrive update is incomplete: the newest release is 99.0.0, and Homebrew has {}. The koidrive/tap tap does not have Koidrive 99.0.0 yet. To update, run: {} upgrade koidrive",
            env!("CARGO_PKG_VERSION"),
            prefix.join("bin/brew").display()
        )
    );
    assert_eq!(envelope["data"]["service"]["state"], "stopped");
    assert!(!output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("is the newest release"));
}

#[test]
fn koi_update_names_the_brew_failure_when_brew_removed_koi_before_it_failed() {
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    scripted_brew(
        &prefix,
        0,
        &format!("  /bin/rm '{}'\n  exit 1", koi.display()),
    );
    let output = koi_at(&link, home.path(), &github, &["update", "--json"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    assert_eq!(
        envelope["error"]["message"],
        format!(
            "brew upgrade failed (exit status 1). To update, run: {} upgrade koidrive",
            prefix.join("bin/brew").display()
        )
    );
}

#[test]
fn koi_update_hands_the_service_to_a_koi_brew_upgraded_before_it_failed() {
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    scripted_brew(
        &prefix,
        0,
        &format!(
            "{}\n  echo 'Error: The brew link step did not complete successfully' >&2\n  exit 1",
            replacing(&koi, "99.0.0", REPORT)
        ),
    );
    let output = koi_at(&link, home.path(), &github, &["update", "--json"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    assert_eq!(
        envelope["error"]["message"],
        format!(
            "brew upgrade failed (exit status 1) after installing Koidrive 99.0.0. To update, run: {} upgrade koidrive",
            prefix.join("bin/brew").display()
        )
    );
    assert_eq!(envelope["data"]["to"], "99.0.0");
    assert_eq!(envelope["data"]["result"], "updated");
    assert!(
        std::fs::read_to_string(koi.with_extension("calls"))
            .unwrap()
            .contains("_reconcile --json\n")
    );
}

#[test]
fn koi_update_uses_the_custom_homebrew_of_the_running_koi_without_a_prefix_variable() {
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let prefix = home.path().join("custombrew");
    let keg = prefix.join("Cellar/koidrive/0.1.0/bin/koi");
    copy_koi(&keg);
    let opt = prefix.join("opt/koidrive");
    std::fs::create_dir_all(opt.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(keg.parent().unwrap().parent().unwrap(), &opt).unwrap();
    let canonical = std::fs::canonicalize(&prefix).unwrap();
    let calls = upgrading_brew(
        &canonical,
        &std::fs::canonicalize(&keg).unwrap(),
        "99.0.0",
        0,
    );
    let output = koi_at(&keg, home.path(), &github, &["update", "--json"])
        .env("PATH", home.path().join("empty"))
        .env_remove("HOMEBREW_PREFIX")
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        assert!(!calls.exists());
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["data"]["via"], "homebrew");
    assert_eq!(envelope["data"]["to"], "99.0.0");
    assert_eq!(
        std::fs::read_to_string(calls).unwrap(),
        "--repository koidrive/tap|1\nupgrade koidrive|1\n"
    );
    assert_eq!(github.requests.lock().unwrap().len(), 1);
}

#[test]
fn homebrew_guidance_targets_its_owner_when_path_selects_another_brew() {
    let home = Sandbox::new();
    let root = home.path().join("owning copy's home");
    let github = github("v99.0.0");
    let binary = homebrew_binary(&root, false);
    let helper_before = std::fs::read(&binary).unwrap();
    let link_before = std::fs::read_link(&binary).unwrap();
    let owner = std::fs::canonicalize(root.join("brew/bin/brew")).unwrap();
    executable(&owner, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\n");
    let other_bin = home.path().join("other/bin");
    let other = other_bin.join("brew");
    executable(
        &other,
        "#!/bin/sh\necho selected >> \"$0.calls\"\nexit 99\n",
    );
    let run = |args: &[&str]| {
        let mut command = koi_at(&binary, &root, &github, args);
        command
            .env("PATH", &other_bin)
            .env("XDG_RUNTIME_DIR", home.path().join("run"));
        command
    };
    if development_version() {
        return;
    }
    let args = ["upgrade", "koidrive"];
    let mut argv = vec![owner.to_str().unwrap()];
    argv.extend(args.iter().copied());
    let command = format!(
        "'{}' {}",
        owner.to_str().unwrap().replace('\'', "'\\''"),
        args.join(" ")
    );
    let state = root.join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("update-check.json"),
        serde_json::json!({
            "asset": asset(), "checked_at": 1763006400000_i64,
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        })
        .to_string(),
    )
    .unwrap();
    let status = run(&["status", "--json"]).output().unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
    assert_eq!(envelope(&status)["data"]["update"]["command"], command);
    let status = run(&["status"]).output().unwrap();
    assert!(
        String::from_utf8(status.stdout)
            .unwrap()
            .contains(&format!("Run: {command}\n"))
    );
    assert!(!owner.with_extension("calls").exists());
    assert!(!other.with_extension("calls").exists());
    assert!(github.requests.lock().unwrap().is_empty());
    std::fs::remove_file(&other).unwrap();
    std::os::unix::fs::symlink(&owner, &other).unwrap();
    let status = run(&["status", "--json"]).output().unwrap();
    assert_eq!(envelope(&status)["data"]["update"]["command"], command);
    let status = sandboxed(argv[0], home.path())
        .args(&argv[1..])
        .env("PATH", &other_bin)
        .status()
        .unwrap();
    assert!(status.success());
    let output = sandboxed("/bin/sh", home.path())
        .args(["-c", &command])
        .env("PATH", &other_bin)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(owner.with_extension("calls")).unwrap(),
        format!("{}\n", args.join(" ")).repeat(2)
    );
    assert!(!other.with_extension("calls").exists());
    assert_eq!(std::fs::read(&binary).unwrap(), helper_before);
    assert_eq!(std::fs::read_link(&binary).unwrap(), link_before);
    assert!(!update_log(&root).exists());
}

#[test]
fn a_relocated_formula_is_found_from_the_process_invocation() {
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let prefix = home.path().join("brew");
    let version = prefix.join("Cellar/koidrive/0.1.0");
    let external = home.path().join("external/koidrive/0.1.0");
    std::fs::create_dir_all(external.parent().unwrap()).unwrap();
    std::fs::rename(&version, &external).unwrap();
    std::fs::write(external.join("INSTALL_RECEIPT.json"), "{}").unwrap();
    let opt = prefix.join("opt/koidrive");
    std::fs::remove_file(&opt).unwrap();
    std::os::unix::fs::symlink(&external, &opt).unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("../opt/koidrive/bin/koi", &link).unwrap();
    let alias = home.path().join("alias");
    std::os::unix::fs::symlink(&link, &alias).unwrap();
    let before = std::fs::read(&alias).unwrap();
    let calls = upgrading_brew(
        &std::fs::canonicalize(&prefix).unwrap(),
        &std::fs::canonicalize(&alias).unwrap(),
        "99.0.0",
        1,
    );
    for invoked in [alias.as_path(), Path::new("./alias"), Path::new("koi")] {
        let path = if invoked == Path::new("koi") {
            prefix.join("bin")
        } else {
            home.path().join("no-tools")
        };
        let output = koi_at(invoked, home.path(), &github, &["update", "--json"])
            .current_dir(home.path())
            .env_remove("HOMEBREW_PREFIX")
            .env("PATH", &path)
            .output()
            .unwrap();
        if development_update(&output, home.path(), &github) {
            continue;
        }
        let response = envelope(&output);
        assert!(!output.status.success(), "{response:#}");
        let owner = std::fs::canonicalize(prefix.join("bin"))
            .unwrap()
            .join("brew");
        let brew = if invoked == Path::new("koi") {
            "brew"
        } else {
            owner.to_str().unwrap()
        };
        assert_eq!(
            response["error"]["message"],
            format!("brew upgrade failed (exit status 1). To update, run: {brew} upgrade koidrive")
        );
    }
    assert_eq!(std::fs::read(&alias).unwrap(), before);
    assert_eq!(std::fs::read_link(&alias).unwrap(), link);
    match development_version() {
        true => assert!(!calls.exists()),
        false => assert_eq!(
            std::fs::read_to_string(&calls).unwrap(),
            "--repository koidrive/tap|1\nupgrade koidrive|1\n".repeat(3)
        ),
    }
    assert_eq!(
        github.requests.lock().unwrap().len(),
        if development_version() { 0 } else { 3 }
    );
}

#[test]
fn a_formula_receipt_with_no_discoverable_prefix_never_self_replaces() {
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let keg = home.path().join("external/package/0.1.0");
    let binary = copy_koi(&keg.join("bin/koi"));
    std::fs::write(keg.join("INSTALL_RECEIPT.json"), "{}").unwrap();
    let alias = home.path().join("direct-store-alias");
    std::os::unix::fs::symlink(&binary, &alias).unwrap();
    let before = std::fs::read(&binary).unwrap();
    for invoked in [&binary, &alias] {
        let output = koi_at(invoked, home.path(), &github, &["update", "--json"])
            .env_remove("HOMEBREW_PREFIX")
            .env("PATH", home.path().join("no-tools"))
            .env("KOIDRIVE_UPDATE_CHECK_URL", "not-a-URL")
            .output()
            .unwrap();
        if development_update(&output, home.path(), &github) {
            continue;
        }
        let response = envelope(&output);
        assert!(!output.status.success(), "{response:#}");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Homebrew"),
            "{response:#}"
        );
    }
    assert_eq!(std::fs::read(&binary).unwrap(), before);
    assert!(!update_log(home.path()).exists());
    assert!(github.requests.lock().unwrap().is_empty());
}

#[test]
fn an_unverified_invocation_never_enters_installer_replacement() {
    for relative in [
        "standalone/koi",
        "Applications/Koidrive.app/Contents/Helpers/koi",
        "Custom Apps/Koidrive.app/Contents/Helpers/koi",
    ] {
        let home = Sandbox::new();
        let github = github("v99.0.0");
        let binary = copy_koi(&home.path().join(relative));
        let before = std::fs::read(&binary).unwrap();
        let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
            .arg0("missing-invocation")
            .env("PATH", home.path().join("no-tools"))
            .env("KOIDRIVE_UPDATE_CHECK_URL", "not-a-URL")
            .output()
            .unwrap();
        if !development_update(&output, home.path(), &github) {
            let response = envelope(&output);
            assert!(!output.status.success(), "{response:#}");
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("run its full path"),
                "{response:#}"
            );
        }
        assert_eq!(std::fs::read(&binary).unwrap(), before);
        assert!(!update_log(home.path()).exists());
        assert!(github.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn adversarial_a_failed_tap_refresh_is_never_reported_as_the_newest_release() {
    if development_version() {
        return;
    }
    let mut claims = Vec::new();
    for json in [false, true] {
        let home = Sandbox::new();
        let github = github("v99.0.0");
        let link = homebrew_binary(home.path(), false);
        let koi = std::fs::canonicalize(&link).unwrap();
        let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
        let same = format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo 'koi {}' ;;\n_reconcile) echo '{REPORT}' ;;\nesac\n",
            env!("CARGO_PKG_VERSION")
        )
        .replace('\'', "'\\''");
        adversarial_brew(
            &prefix,
            1,
            &format!(
                "  printf '%s' '{same}' > '{koi}.new' && /bin/chmod +x '{koi}.new' && /bin/mv '{koi}.new' '{koi}'\n  echo 'Warning: koidrive/tap/koidrive {} already installed' >&2\n  exit 0",
                env!("CARGO_PKG_VERSION"),
                koi = koi.display()
            ),
        );
        let args: &[&str] = if json {
            &["update", "--json"]
        } else {
            &["update"]
        };
        let output = koi_at(&link, home.path(), &github, args)
            .env("PATH", home.path().join("empty"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        eprintln!(
            "json={json} exit: {:?}\n--- stdout\n{stdout}--- stderr\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        if output.status.success()
            && (stdout.contains("is the newest release")
                || stdout.contains("\"outcome\": \"unchanged\""))
        {
            claims.push(json);
        }
    }
    assert!(
        claims.is_empty(),
        "koi update succeeds as up to date after the tap refresh failed, while v99.0.0 is published (json modes: {claims:?})"
    );
}

#[test]
fn adversarial_b_brew_failing_after_it_upgraded_koi_still_moves_the_service() {
    if development_version() {
        return;
    }
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    let upgraded = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi 99.0.0' ;;\n_reconcile) echo '{REPORT}' ;;\nesac\n"
    )
    .replace('\'', "'\\''");
    adversarial_brew(
        &prefix,
        0,
        &format!(
            "  printf '%s' '{upgraded}' > '{koi}.new' && /bin/chmod +x '{koi}.new' && /bin/mv '{koi}.new' '{koi}'\n  echo 'Error: The brew link step did not complete successfully' >&2\n  exit 1",
            koi = koi.display()
        ),
    );
    let output = koi_at(&link, home.path(), &github, &["update"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!(
        "exit: {:?}\n--- stdout\n{stdout}--- stderr\n{stderr}",
        output.status.code()
    );
    let version = koi_at(&koi, home.path(), &github, &["--version"])
        .output()
        .unwrap();
    eprintln!(
        "installed koi now: {}",
        String::from_utf8_lossy(&version.stdout)
    );
    let calls = std::fs::read_to_string(koi.with_extension("calls")).unwrap_or_default();
    assert!(
        calls.contains("_reconcile"),
        "koi 99.0.0 is installed but the service was never handed to it; koi calls: {calls:?}"
    );
}

#[test]
fn koi_update_never_runs_a_homebrew_koi_outside_its_sandbox() {
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let outside = tempfile::tempdir().unwrap();
    let ran = outside.path().join("ran");
    let real = outside.path().join("koi");
    executable(
        &real,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in\n--version) echo 'koi 99.0.0' ;;\n_reconcile) echo '{REPORT}' ;;\nesac\n",
            ran.display()
        ),
    );
    let opt = home.path().join("brew/opt/koidrive");
    std::fs::remove_file(&opt).unwrap();
    std::fs::create_dir_all(opt.join("bin")).unwrap();
    std::os::unix::fs::symlink(&real, opt.join("bin/koi")).unwrap();
    let output = koi_at(&link, home.path(), &github, &["update", "--json"])
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let calls = std::fs::read_to_string(&ran).unwrap_or_default();
    assert!(calls.is_empty(), "outside koi ran: {calls:?}");
}

#[test]
fn koi_update_never_pulls_a_homebrew_tap_outside_its_sandbox() {
    let home = Sandbox::new();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    upgrading_brew(&prefix, &koi, "99.0.0", 0);
    std::fs::remove_file(prefix.join("bin/git")).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let upstream = outside.path().join("upstream");
    let tap = outside.path().join("tap");
    let ran = outside.path().join("ran");
    let sh = |script: &str| {
        let output = sandboxed("/bin/sh", home.path())
            .args(["-c", script])
            .env("HOME", outside.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{script}");
        String::from_utf8(output.stdout).unwrap()
    };
    let commit = "git -c user.name=a -c user.email=a@a commit -q --allow-empty";
    let hook = tap.join(".git/hooks/post-merge");
    sh(&format!(
        "set -e; git init -q '{u}'; cd '{u}'; {commit} -m one; git clone -q '{u}' '{t}'; {commit} -m two; printf '#!/bin/sh\\ntouch \"%s\"\\n' '{r}' > '{h}'; chmod +x '{h}'",
        u = upstream.display(),
        t = tap.display(),
        r = ran.display(),
        h = hook.display(),
    ));
    let head = || sh(&format!("git -C '{}' rev-parse HEAD", tap.display()));
    let before = head();
    let brew = prefix.join("bin/brew");
    let script = std::fs::read_to_string(&brew).unwrap().replace(
        &prefix
            .join("Library/Taps/koidrive/homebrew-tap")
            .display()
            .to_string(),
        &tap.display().to_string(),
    );
    std::fs::write(&brew, script).unwrap();
    let output = koi_at(&link, home.path(), &github, &["update", "--json"])
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    assert!(!ran.exists(), "{:#}", envelope(&output));
    assert_eq!(head(), before);
}
