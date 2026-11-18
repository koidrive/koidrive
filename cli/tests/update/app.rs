// SPDX-License-Identifier: Apache-2.0

#[test]
fn an_app_outside_the_installer_inventory_never_updates_another_bundle() {
    custom_app_update("v99.0.0", true);
}

#[test]
fn an_app_outside_the_installer_inventory_is_unchanged_at_the_target() {
    custom_app_update(&format!("v{}", env!("CARGO_PKG_VERSION")), false);
}

#[test]
fn an_app_outside_the_installer_inventory_is_unchanged_above_the_target() {
    custom_app_update("v0.0.1", false);
}

fn custom_app_update(tag: &str, upgrading: bool) {
    let home = tempfile::tempdir().unwrap();
    let github = github(tag);
    let binary = copy_koi(
        &home
            .path()
            .join("Custom Apps/Koidrive.app/Contents/Helpers/koi"),
    );
    let other = home
        .path()
        .join("Applications/Koidrive.app/Contents/Helpers/koi");
    executable(
        &other,
        "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 99.0.0'\n",
    );
    let before = std::fs::read(&binary).unwrap();
    let other_before = std::fs::read(&other).unwrap();
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let response = envelope(&output);
    if upgrading {
        assert!(!output.status.success(), "{response:#}");
        assert_eq!(response["outcome"], "error");
        assert_eq!(
            response["error"]["message"],
            "Download Koidrive 99.0.0 from https://github.com/koidrive/koidrive/releases/tag/v99.0.0"
        );
    } else {
        assert!(output.status.success(), "{response:#}");
        assert_eq!(response["outcome"], "unchanged");
        assert_eq!(
            response["data"],
            serde_json::json!({
                "from": env!("CARGO_PKG_VERSION"),
                "to": env!("CARGO_PKG_VERSION"),
                "via": "app",
                "result": "unchanged",
            })
        );
    }
    assert_eq!(response["next_commands"], serde_json::json!([]));
    assert_eq!(std::fs::read(&binary).unwrap(), before);
    assert_eq!(std::fs::read(&other).unwrap(), other_before);
    assert!(!other.with_extension("calls").exists());
    assert!(!update_log(home.path()).exists());
    assert_eq!(github.requests.lock().unwrap().len(), 1);
}

#[test]
fn an_app_outside_the_installer_inventory_uses_the_running_version_without_probing_its_path() {
    if development_version() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let binary = copy_koi(
        &home
            .path()
            .join("Custom Apps/Koidrive.app/Contents/Helpers/koi"),
    );
    let replacement = binary.clone();
    let releases = serde_json::json!([{
        "tag_name": format!("v{}", env!("CARGO_PKG_VERSION")),
        "html_url": "https://example.com/current",
        "assets": [{ "name": asset() }, { "name": "SHA256SUMS" }],
    }]);
    let github = http_before_reply(releases.to_string(), move || {
        let staged = replacement.with_extension("new");
        executable(
            &staged,
            "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 0.0.1'\n",
        );
        std::fs::rename(staged, &replacement).unwrap();
    });
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .output()
        .unwrap();
    let response = envelope(&output);
    assert!(output.status.success(), "{response:#}");
    assert_eq!(response["outcome"], "unchanged");
    assert_eq!(
        response["data"],
        serde_json::json!({
            "from": env!("CARGO_PKG_VERSION"),
            "to": env!("CARGO_PKG_VERSION"),
            "via": "app",
            "result": "unchanged",
        })
    );
    assert!(!binary.with_extension("calls").exists());
    assert!(!update_log(home.path()).exists());
    assert_eq!(github.requests.lock().unwrap().len(), 1);
}

#[test]
fn an_app_outside_the_installer_inventory_status_names_the_download_not_an_update_command() {
    if development_version() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(
        &home
            .path()
            .join("Custom Apps/Koidrive.app/Contents/Helpers/koi"),
    );
    let other = home
        .path()
        .join("Applications/Koidrive.app/Contents/Helpers/koi");
    executable(
        &other,
        "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 99.0.0'\n",
    );
    let before = std::fs::read(&binary).unwrap();
    let other_before = std::fs::read(&other).unwrap();
    let state = home.path().join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    let url = "https://github.com/koidrive/koidrive/releases/tag/v99.0.0";
    std::fs::write(
        state.join("update-check.json"),
        serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "release": { "version": "99.0.0", "url": url },
        })
        .to_string(),
    )
    .unwrap();
    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let response = envelope(&output);
    let update = &response["data"]["update"];
    assert_eq!(update["state"], "available");
    assert_eq!(update["url"], url);
    assert!(update.get("command").is_none(), "{update:#}");
    assert_eq!(
        response["next_commands"],
        serde_json::json!([{ "argv": ["koi", "setup"] }])
    );
    let output = koi_at(&binary, home.path(), &github, &["status"])
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(&format!("Download: {url}")), "{text}");
    assert!(!text.contains("Run:"), "{text}");
    assert!(github.requests.lock().unwrap().is_empty());
    assert!(!update_log(home.path()).exists());
    assert_eq!(std::fs::read(&binary).unwrap(), before);
    assert_eq!(std::fs::read(&other).unwrap(), other_before);
    assert!(!other.with_extension("calls").exists());
}

#[test]
fn koi_update_keeps_a_direct_app_helper_on_the_manual_download_path() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(
        &home
            .path()
            .join("Applications/Koidrive.app/Contents/Helpers/koi"),
    );
    let before = std::fs::read(&binary).unwrap();
    let (script, installer) = script_install(home.path(), "99.0.0", REPORT);
    let script_before = std::fs::read(&script).unwrap();
    let calls = homebrew(home.path(), "100.0.0");
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let response = envelope(&output);
    assert!(!output.status.success(), "{response:#}");
    assert_eq!(response["outcome"], "error");
    let message = response["error"]["message"].as_str().unwrap();
    assert!(
        message.ends_with(
            "Download Koidrive 99.0.0 from https://github.com/koidrive/koidrive/releases/tag/v99.0.0"
        ),
        "{message}"
    );
    if cfg!(target_os = "macos") {
        assert!(
            message.starts_with("App signature verification failed"),
            "{message}"
        );
    }
    assert_eq!(response["next_commands"], serde_json::json!([]));
    assert_eq!(github.requests.lock().unwrap().len(), 1);
    assert!(installer.requests.lock().unwrap().is_empty());
    assert!(!calls.exists());
    assert!(!script.with_extension("calls").exists());
    assert!(!binary.with_extension("calls").exists());
    assert_eq!(std::fs::read(&binary).unwrap(), before);
    assert_eq!(std::fs::read(&script).unwrap(), script_before);
    let state = home.path().join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("update-check.json"),
        serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "release": {
                "version": "99.0.0",
                "url": "https://github.com/koidrive/koidrive/releases/tag/v99.0.0",
            },
        })
        .to_string(),
    )
    .unwrap();
    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let update = envelope(&output)["data"]["update"].clone();
    assert_eq!(update["state"], "available");
    let command = format!(
        "{} update",
        std::fs::canonicalize(&binary).unwrap().display()
    );
    assert_eq!(update["command"], command, "{update:#}");
    let output = koi_at(&binary, home.path(), &github, &["status"])
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(&format!("Run: {command}")), "{text}");
    assert!(!text.contains("Run: koi update"), "{text}");
    assert_eq!(github.requests.lock().unwrap().len(), 1);
}
