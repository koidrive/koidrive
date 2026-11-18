// SPDX-License-Identifier: Apache-2.0

/// Needs a systemd user instance, which the CI job starts by turning lingering on, and `KOIDRIVE_TEST_SYSTEMD=1`.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker and a systemd user instance; run cli/scripts/integration-test.sh"]
async fn a_linked_folder_syncs_in_the_systemd_user_service_until_the_last_unlink() {
    if std::env::var_os("KOIDRIVE_TEST_SYSTEMD").is_none() {
        eprintln!("skipped: set KOIDRIVE_TEST_SYSTEMD=1 where systemctl --user works");
        return;
    }
    let _systemd = SYSTEMD.lock().await;
    let systemctl = |args: &[&str]| {
        Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .unwrap()
    };
    let owner = Auth::new(api(), owner().await);
    let space_name = format!("background {}", uuidv7());
    owner
        .api()
        .create_space(
            &owner,
            &CreateSpaceRequest {
                name: space_name.clone(),
                request_id: String::new(),
            },
        )
        .await
        .unwrap();
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let machine = Machine::with_service();
    let server = server_url().to_string();
    machine.run(&[
        "join",
        &pairing.code,
        "--server",
        &server,
        "--name",
        "server-vm",
    ]);
    let folder = machine.path("Sync");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("notes.txt"), b"synced by the service").unwrap();
    let folder = folder.to_str().unwrap();

    let linked = machine.data(&["folder", "link", folder, "--space", &space_name]);
    let service = &linked["service"];
    assert_eq!(service["manager"], "systemd", "{linked:#}");
    assert_eq!(service["state"], "running", "{linked:#}");
    assert_eq!(service["lingers"], true, "{linked:#}");
    let unit = service["name"].as_str().unwrap().to_owned();
    assert!(unit.starts_with("koidrive-"), "{unit}");
    assert!(systemctl(&["is-active", "--quiet", &unit]).status.success());
    assert!(
        systemctl(&["is-enabled", "--quiet", &unit])
            .status
            .success()
    );
    let main_pid = || systemctl(&["show", "--property=MainPID", "--value", &unit]).stdout;
    let pid = main_pid();
    let status = machine
        .status_until("the service syncing the folder", |status| {
            status["running"] == true && status["folders"][0]["state"] == "up_to_date"
        })
        .await;
    assert_eq!(status["service"]["state"], "running", "{status:#}");

    let restarted = machine.data(&["folder", "restart", folder]);
    assert!(restarted.get("service").is_none(), "{restarted:#}");
    let started = machine.data(&["service", "start"]);
    assert_eq!(started["service"]["state"], "running", "{started:#}");
    assert_eq!(main_pid(), pid, "a second reconcile restarted the service");

    let unlinked = machine.data(&["folder", "unlink", folder]);
    assert_eq!(
        unlinked["service"]["state"], "not_installed",
        "{unlinked:#}"
    );
    assert!(!systemctl(&["is-active", "--quiet", &unit]).status.success());
    assert!(!systemctl(&["cat", &unit]).status.success());
    assert!(!machine.path("state/koidrive/systemd").join(&unit).exists());
    assert_eq!(machine.status()["service"]["state"], "not_installed");
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker and a systemd user instance; run cli/scripts/integration-test.sh"]
async fn uninstall_signs_out_removes_the_service_and_keeps_every_synced_file() {
    if std::env::var_os("KOIDRIVE_TEST_SYSTEMD").is_none() {
        eprintln!("skipped: set KOIDRIVE_TEST_SYSTEMD=1 where systemctl --user works");
        return;
    }
    let _systemd = SYSTEMD.lock().await;
    let systemctl = |args: &[&str]| {
        Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .unwrap()
    };
    let owner = Arc::new(Auth::new(api(), owner().await));
    let space_name = format!("uninstall {}", uuidv7());
    let space = owner
        .api()
        .create_space(
            &owner,
            &CreateSpaceRequest {
                name: space_name.clone(),
                request_id: String::new(),
            },
        )
        .await
        .unwrap();
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let mut machine = Machine::with_service();
    machine.install();
    let server = server_url().to_string();
    machine.run(&[
        "join",
        &pairing.code,
        "--server",
        &server,
        "--name",
        "leaving",
    ]);
    let device_id = machine.status()["account"]["device_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let folder = machine.path("Koidrive");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("Budget.xlsx"), b"written here").unwrap();
    let linked = machine.data(&[
        "folder",
        "link",
        folder.to_str().unwrap(),
        "--space",
        &space_name,
    ]);
    let unit = linked["service"]["name"].as_str().unwrap().to_owned();
    assert!(unit.starts_with("koidrive-"), "{unit}");
    assert!(systemctl(&["is-active", "--quiet", &unit]).status.success());
    machine
        .status_until("the folder following the space's log", |status| {
            status["folders"][0]["last_seq"].is_u64()
        })
        .await;
    commit_as_owner(
        &owner,
        &space.space_id,
        &[b"written on another device"],
        vec![op::Op::CreateFile(CreateFile {
            made_over: Vec::new(),
            source_version_id: String::new(),
            node_id: uuidv7(),
            parent_id: space.space_id.clone(),
            name: "Slides.pptx".into(),
            content: Some(file_content(b"written on another device")),
            ..Default::default()
        })],
    )
    .await;
    let synced = folder.join("Slides.pptx");
    eventually("the service syncing a file down", || {
        fs::read(&synced).is_ok_and(|bytes| bytes == b"written on another device")
    })
    .await;
    machine
        .status_until("the folder up to date", |status| {
            status["folders"][0]["state"] == "up_to_date"
        })
        .await;

    let uninstalled = machine.envelope(&["uninstall", "--yes"]);

    let data = &uninstalled["data"];
    assert_eq!(
        data["device"],
        serde_json::json!({ "state": "signed_out", "server": server }),
        "{uninstalled:#}"
    );
    let removed = data["removed"].as_array().unwrap();
    for item in [
        serde_json::json!({ "kind": "service", "name": unit }),
        serde_json::json!({ "kind": "command", "path": machine.koi.display().to_string() }),
        serde_json::json!({ "kind": "folder_data", "path": folder.join(".koidrive").display().to_string() }),
    ] {
        assert!(removed.contains(&item), "{item} in {uninstalled:#}");
    }
    assert!(
        data["kept"].as_array().unwrap().contains(
            &serde_json::json!({ "kind": "folder", "path": folder.display().to_string() })
        ),
        "{uninstalled:#}"
    );
    assert!(!systemctl(&["is-active", "--quiet", &unit]).status.success());
    assert!(!systemctl(&["cat", &unit]).status.success());
    let mut left: Vec<String> = fs::read_dir(machine.home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    left.retain(|name| name != "config");
    assert_eq!(left, [".local", "Koidrive", "state"], "{uninstalled:#}");
    for dir in ["config", "state", ".local/bin"] {
        let entries = match fs::read_dir(machine.path(dir)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            entries => entries.unwrap().map(Result::unwrap).count(),
        };
        assert_eq!(entries, 0, "{dir}");
    }
    let mut files: Vec<String> = fs::read_dir(&folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(files, ["Budget.xlsx", "Slides.pptx"]);
    assert_eq!(
        fs::read(folder.join("Budget.xlsx")).unwrap(),
        b"written here"
    );
    assert_eq!(fs::read(&synced).unwrap(), b"written on another device");
    let devices = owner.api().list_devices(&owner, "").await.unwrap().devices;
    let listed = devices
        .iter()
        .find(|device| device.device_id == device_id)
        .unwrap();
    assert!(listed.revoked_at_ms > 0, "{listed:?}");

    machine.koi = PathBuf::from(env!("CARGO_BIN_EXE_koi"));
    let again = machine.envelope(&["uninstall", "--yes"]);
    assert_eq!(again["outcome"], "unchanged", "{again:#}");
}
