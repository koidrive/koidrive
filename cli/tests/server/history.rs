// SPDX-License-Identifier: Apache-2.0

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn trash_and_versions_list_what_the_space_keeps() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let space_name = format!("history {}", uuidv7());
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
    let server = server_url().to_string();
    let machine = Machine::new();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let folder = machine.path("Sync");
    fs::create_dir_all(&folder).unwrap();
    let folder_arg = folder.to_str().unwrap();
    machine.run(&["folder", "link", folder_arg, "--space", &space_name]);
    let daemon = machine
        .command(&["_daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = machine.socket();
    let started = Instant::now();
    while !socket.exists() {
        assert!(
            started.elapsed() < WAIT,
            "the daemon did not open its socket"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    machine
        .status_until("the first sync", |status| {
            status["folders"][0]["state"] == "up_to_date"
        })
        .await;

    let docs = uuidv7();
    let note = uuidv7();
    let gone = uuidv7();
    commit_as_owner(
        &owner,
        &space.space_id,
        &[b"first", b"deleted later", b"link", b"target text"],
        vec![
            op::Op::CreateDir(CreateDir {
                node_id: docs.clone(),
                parent_id: space.space_id.clone(),
                name: "docs".into(),
                pending_for: String::new(),
            }),
            op::Op::CreateFile(CreateFile {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id: note.clone(),
                parent_id: docs.clone(),
                name: "note.txt".into(),
                content: Some(file_content(b"first")),
                ..Default::default()
            }),
            op::Op::CreateFile(CreateFile {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id: gone.clone(),
                parent_id: docs.clone(),
                name: "gone.txt".into(),
                content: Some(file_content(b"deleted later")),
                ..Default::default()
            }),
            op::Op::CreateFile(CreateFile {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id: uuidv7(),
                parent_id: docs.clone(),
                name: "link.txt".into(),
                content: Some(file_content(b"link")),
                ..Default::default()
            }),
            op::Op::CreateFile(CreateFile {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id: uuidv7(),
                parent_id: docs.clone(),
                name: "target.txt".into(),
                content: Some(file_content(b"target text")),
                ..Default::default()
            }),
        ],
    )
    .await;
    let empty = machine.run(&["history", "trash", folder_arg]);
    assert!(empty.contains("Nothing from"), "{empty}");

    commit_as_owner(
        &owner,
        &space.space_id,
        &[b"second!"],
        vec![
            op::Op::UpdateContent(UpdateContent {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id: note,
                content: Some(file_content(b"second!")),
                resend: false,
            }),
            op::Op::Delete(Delete {
                node_id: gone,
                requires: None,
                ..Default::default()
            }),
        ],
    )
    .await;
    let note_path = folder.join("docs/note.txt");
    let note_arg = note_path.to_str().unwrap();
    let versions: Value = machine.data(&["history", "versions", note_arg])["versions"].clone();
    assert_eq!(versions.as_array().unwrap().len(), 2, "{versions:#}");
    assert_eq!(versions[0]["size"], 7, "{versions:#}");
    assert_eq!(versions[1]["size"], 5, "{versions:#}");
    let trash: Value = machine.data(&["history", "trash", folder_arg])["entries"].clone();
    assert_eq!(trash.as_array().unwrap().len(), 1, "{trash:#}");
    assert_eq!(trash[0]["path"], "docs/gone.txt", "{trash:#}");
    assert_eq!(trash[0]["kind"], "file");
    assert_eq!(trash[0]["bytes"], 13);
    let below = machine.run(&["history", "trash", folder.join("docs").to_str().unwrap()]);
    assert!(below.contains("docs/gone.txt"), "{below}");
    let report = machine.run(&["history", "versions", note_arg]);
    assert!(
        report.starts_with("docs/note.txt: 2 versions, newest first\ncurrent"),
        "{report}"
    );
    let deleted = machine.fails(&[
        "history",
        "versions",
        folder.join("docs/gone.txt").to_str().unwrap(),
    ]);
    assert!(
        deleted.contains("koi history trash lists deleted ones"),
        "{deleted}"
    );
    let outside = machine.fails(&["history", "trash", machine.path("state").to_str().unwrap()]);
    assert!(outside.contains("not in a linked folder"), "{outside}");
    let link = folder.join("docs/link.txt");
    let started = Instant::now();
    while fs::read(&link).ok().as_deref() != Some(&b"link"[..]) {
        assert!(
            started.elapsed() < WAIT,
            "the daemon did not download docs/link.txt"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(stop(daemon).success());

    let profile = machine.profile();
    fs::remove_file(&link).unwrap();
    let link_arg = link.to_str().unwrap();
    let size_of_link = |what: &str| {
        let versions: Value = machine.data(&["history", "versions", link_arg])["versions"].clone();
        assert_eq!(
            versions.as_array().unwrap().len(),
            1,
            "{what}: {versions:#}"
        );
        versions[0]["size"].clone()
    };
    std::os::unix::fs::symlink("target.txt", &link).unwrap();
    assert_eq!(size_of_link("a link to target.txt"), 11);
    fs::remove_file(&link).unwrap();
    let elsewhere = machine.path("elsewhere.txt");
    fs::write(&elsewhere, b"not synced").unwrap();
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    let outward = machine.fails(&["history", "versions", link_arg]);
    assert!(outward.contains("not in a linked folder"), "{outward}");
    let stopped = machine.run(&["history", "trash", folder_arg, "--json"]);
    assert!(stopped.contains("docs/gone.txt"), "{stopped}");
    let unlinked = machine.fails(&[
        "history",
        "trash",
        machine.path("elsewhere").to_str().unwrap(),
    ]);
    assert!(unlinked.contains("not in a linked folder"), "{unlinked}");
    let unlinked = machine.fails(&["history", "versions", elsewhere.to_str().unwrap()]);
    assert!(unlinked.contains("not in a linked folder"), "{unlinked}");
    assert!(
        machine.profile() == profile,
        "the commands changed the profile without a daemon"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn restore_brings_back_a_deleted_file_and_an_earlier_version() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let space_name = format!("restore {}", uuidv7());
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
    let server = server_url().to_string();
    let machine = Machine::new();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let folder = machine.path("Sync");
    fs::create_dir_all(&folder).unwrap();
    let folder_arg = folder.to_str().unwrap();
    machine.run(&["folder", "link", folder_arg, "--space", &space_name]);
    let daemon = machine
        .command(&["_daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = machine.socket();
    eventually("the daemon to open its socket", || socket.exists()).await;
    machine
        .status_until("the first sync", |status| {
            status["folders"][0]["state"] == "up_to_date"
        })
        .await;

    let docs = uuidv7();
    let note = uuidv7();
    let gone = uuidv7();
    commit_as_owner(
        &owner,
        &space.space_id,
        &[b"first", b"deleted later"],
        vec![
            op::Op::CreateDir(CreateDir {
                node_id: docs.clone(),
                parent_id: space.space_id.clone(),
                name: "docs".into(),
                pending_for: String::new(),
            }),
            op::Op::CreateFile(CreateFile {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id: note.clone(),
                parent_id: docs.clone(),
                name: "note.txt".into(),
                content: Some(file_content(b"first")),
                ..Default::default()
            }),
            op::Op::CreateFile(CreateFile {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id: gone.clone(),
                parent_id: docs,
                name: "gone.txt".into(),
                content: Some(file_content(b"deleted later")),
                ..Default::default()
            }),
        ],
    )
    .await;
    let note_path = folder.join("docs/note.txt");
    let gone_path = folder.join("docs/gone.txt");
    eventually("docs/gone.txt to arrive", || {
        fs::read(&gone_path).ok().as_deref() == Some(&b"deleted later"[..])
    })
    .await;
    commit_as_owner(
        &owner,
        &space.space_id,
        &[b"second!"],
        vec![
            op::Op::UpdateContent(UpdateContent {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id: note,
                content: Some(file_content(b"second!")),
                resend: false,
            }),
            op::Op::Delete(Delete {
                node_id: gone,
                requires: None,
                ..Default::default()
            }),
        ],
    )
    .await;
    eventually("the edit and the deletion to arrive", || {
        fs::read(&note_path).ok().as_deref() == Some(&b"second!"[..]) && !gone_path.exists()
    })
    .await;

    let note_arg = note_path.to_str().unwrap();
    let versions: Value = machine.data(&["history", "versions", note_arg])["versions"].clone();
    let current = versions[0]["version_id"].as_str().unwrap();
    let first = versions[1]["version_id"].as_str().unwrap();
    let again = machine.fails(&["history", "restore", note_arg, "--version", current]);
    assert!(again.contains("already the current version"), "{again}");
    let restore_gone = [
        "history",
        "restore",
        gone_path.to_str().unwrap(),
        "--request-id",
        "restore-gone",
    ];
    let asked = machine.output(&restore_gone);
    assert!(asked.status.success(), "{asked:?}");
    let stdout = String::from_utf8(asked.stdout).unwrap();
    let id = stdout
        .strip_suffix('\n')
        .unwrap_or_else(|| panic!("{stdout:?}"));
    assert!(
        !id.is_empty() && !id.contains(char::is_whitespace),
        "only the ID: {stdout:?}"
    );
    let note = String::from_utf8(asked.stderr).unwrap();
    assert!(
        note.starts_with(&format!("Asked to restore docs/gone.txt (restore {id}).\n")),
        "{note}"
    );
    machine.run(&["history", "restore", note_arg, "--version", first]);
    eventually(
        "the deleted file and the first version to come back",
        || {
            fs::read(&gone_path).ok().as_deref() == Some(&b"deleted later"[..])
                && fs::read(&note_path).ok().as_deref() == Some(&b"first"[..])
        },
    )
    .await;
    let restores =
        || -> Value { machine.data(&["history", "requests", folder_arg])["requests"].clone() };
    eventually("both restores to be reported", || {
        restores()
            .as_array()
            .unwrap()
            .iter()
            .filter(|restore| restore["state"]["type"] == "restored")
            .count()
            == 2
    })
    .await;
    let report = machine.run(&["history", "requests", folder_arg]);
    assert!(report.contains("restored"), "{report}");
    assert!(!report.contains("pending"), "{report}");
    let trash = machine.run(&["history", "trash", folder_arg]);
    assert!(trash.contains("Nothing from"), "{trash}");
    assert_eq!(machine.run(&restore_gone), stdout, "the first restore");
    let replayed = machine.envelope(&restore_gone);
    assert_eq!(replayed["outcome"], "accepted", "{replayed:#}");
    assert_eq!(replayed["data"]["request_id"], id);
    assert_eq!(replayed["data"]["path"], "docs/gone.txt");
    let count = restores().as_array().unwrap().len();
    assert_eq!(count, 2, "no second restore");
    let stale = machine.output(&[
        "history",
        "restore",
        note_arg,
        "--request-id",
        "restore-gone",
    ]);
    assert_eq!(stale.status.code(), Some(7), "{stale:?}");
    let stale = String::from_utf8(stale.stderr).unwrap();
    assert!(stale.contains("stale request"), "{stale}");
    machine.run(&["folder", "unlink", folder_arg]);
    let unlinked = machine.envelope(&restore_gone);
    assert_eq!(unlinked["outcome"], "accepted", "{unlinked:#}");
    assert_eq!(unlinked["data"], replayed["data"], "{unlinked:#}");
    assert_eq!(
        unlinked["next_commands"],
        serde_json::json!([]),
        "{unlinked:#}"
    );
    assert!(stop(daemon).success());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_restore_of_an_item_deleted_before_the_first_sync_lists_its_path() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let space_name = format!("deleted early {}", uuidv7());
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
    let root = space.space_id.clone();
    let (docs, early) = (uuidv7(), uuidv7());
    commit_as_owner(
        &owner,
        &root,
        &[b"early"],
        vec![
            op::Op::CreateDir(CreateDir {
                node_id: docs.clone(),
                parent_id: root.clone(),
                name: "docs".into(),
                pending_for: String::new(),
            }),
            op::Op::CreateFile(CreateFile {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id: early.clone(),
                parent_id: docs.clone(),
                name: "early.txt".into(),
                content: Some(file_content(b"early")),
                ..Default::default()
            }),
        ],
    )
    .await;
    let delete = op::Op::Delete(Delete {
        node_id: early.clone(),
        requires: None,
        ..Default::default()
    });
    commit_as_owner(&owner, &root, &[], vec![delete]).await;

    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    let machine = Machine::new();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let folder = machine.path("Sync");
    fs::create_dir_all(&folder).unwrap();
    let folder_arg = folder.to_str().unwrap();
    machine.run(&["folder", "link", folder_arg, "--space", &space_name]);
    let daemon = machine
        .command(&["_daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = machine.socket();
    eventually("the daemon to open its socket", || socket.exists()).await;
    machine
        .status_until("the first sync", |s| {
            s["folders"][0]["state"] == "up_to_date"
        })
        .await;
    assert!(stop(daemon).success());

    let early_path = folder.join("docs/early.txt");
    machine.data(&[
        "history",
        "restore",
        "--node",
        &early,
        early_path.to_str().unwrap(),
    ]);
    let requests = machine.data(&["history", "requests", folder_arg])["requests"].clone();
    assert_eq!(
        requests[0]["request"]["node_id"],
        early.as_str(),
        "{requests:#}"
    );
    assert_eq!(requests[0]["path"], "docs/early.txt", "{requests:#}");
}
