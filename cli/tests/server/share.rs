// SPDX-License-Identifier: Apache-2.0

async fn shared(link_id: &str) -> (u16, Value) {
    let reply = reqwest::Client::new()
        .get(server_url().join(&format!("v1/share/{link_id}")).unwrap())
        .header("Accept", "application/json")
        .send()
        .await
        .unwrap();
    let status = reply.status().as_u16();
    let body = reply.text().await.unwrap();
    (
        status,
        serde_json::from_str(&body).unwrap_or(Value::String(body)),
    )
}

fn link_id(url: &str) -> String {
    url.rsplit_once("/s/").unwrap().1.to_owned()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_share_link_opens_what_it_shares_until_it_is_revoked() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let create = |name: String| {
        let owner = owner.clone();
        async move {
            (owner.api())
                .create_space(
                    &owner,
                    &CreateSpaceRequest {
                        name,
                        request_id: String::new(),
                    },
                )
                .await
                .unwrap()
        }
    };
    let space_name = format!("share {}", uuidv7());
    let space = create(space_name.clone()).await;
    let elsewhere = create(format!("share elsewhere {}", uuidv7())).await;
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let machine = Machine::new();
    let server = server_url().to_string();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let folder = machine.path("Sync");
    fs::create_dir_all(&folder).unwrap();
    let folder = folder.canonicalize().unwrap();
    machine.run(&[
        "folder",
        "link",
        folder.to_str().unwrap(),
        "--space",
        &space_name,
    ]);
    let log = machine.path("daemon.log");
    let daemon = machine
        .command(&["_daemon"])
        .env("KOIDRIVE_LOG", "debug")
        .stdout(Stdio::null())
        .stderr(File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let socket = machine.socket();
    eventually("the daemon's socket", || socket.exists()).await;
    machine
        .status_until("the first sync", |status| {
            status["folders"][0]["state"] == "up_to_date"
        })
        .await;

    let docs = uuidv7();
    commit_as_owner(
        &owner,
        &space.space_id,
        &[b"the plan"],
        vec![
            op::Op::CreateDir(CreateDir {
                node_id: docs.clone(),
                parent_id: space.space_id.clone(),
                name: "Docs".into(),
                pending_for: String::new(),
            }),
            op::Op::CreateFile(CreateFile {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id: uuidv7(),
                parent_id: docs.clone(),
                name: "plan.txt".into(),
                content: Some(file_content(b"the plan")),
                ..Default::default()
            }),
        ],
    )
    .await;
    let away = uuidv7();
    commit_as_owner(
        &owner,
        &elsewhere.space_id,
        &[b"away"],
        vec![op::Op::CreateFile(CreateFile {
            made_over: Vec::new(),
            source_version_id: String::new(),
            node_id: away.clone(),
            parent_id: elsewhere.space_id.clone(),
            name: "away.txt".into(),
            content: Some(file_content(b"away")),
            ..Default::default()
        })],
    )
    .await;
    let plan = folder.join("Docs/plan.txt");
    eventually("the shared file on this device", || plan.exists()).await;

    let created = machine.data(&["share", "create", plan.to_str().unwrap()]);
    let url = created["url"].as_str().unwrap().to_owned();
    let file_link = link_id(&url);
    assert!(url.starts_with(&format!("{server}s/")), "{created:#}");
    assert_eq!(created["path"], "Docs/plan.txt", "{created:#}");
    assert_eq!(created["kind"], "file", "{created:#}");
    assert_eq!(created["state"], "active", "{created:#}");
    assert_eq!(created["folder"], folder.to_str().unwrap(), "{created:#}");
    let week =
        created["expires_at_ms"].as_i64().unwrap() - created["created_at_ms"].as_i64().unwrap();
    assert_eq!(week, 7 * 24 * 3600 * 1000, "{created:#}");
    let (status, info) = shared(&file_link).await;
    assert_eq!(status, 200, "{info:#}");
    assert_eq!(info["name"], "plan.txt", "{info:#}");

    for request in [
        koidrive_ipc::Request::Share {
            path: plan.clone(),
            expiry: koidrive_driver::Expiry::Default,
            password: None,
        },
        koidrive_ipc::Request::ShareLinks,
        koidrive_ipc::Request::RevokeShareLink {
            link: koidrive_driver::Secret(url.clone()),
        },
    ] {
        let request = request.once(
            Some(koidrive_ipc::RequestId::parse("share-record").unwrap()),
            koidrive_ipc::Key::restart(&folder),
        );
        let response = koidrive_ipc::send(&socket, &request, rustix::process::getuid().as_raw())
            .await
            .unwrap();
        assert!(matches!(
            response,
            Some(koidrive_ipc::Response::Error {
                code: koidrive_ipc::Code::Usage,
                ..
            })
        ));
        assert!(!machine.path("state/koidrive/requests").exists());
    }
    assert_eq!(shared(&file_link).await.0, 200);

    let printed = machine.output(&[
        "share",
        "create",
        folder.join("Docs").to_str().unwrap(),
        "--expires",
        "2h",
    ]);
    assert!(printed.status.success(), "{printed:?}");
    let stdout = String::from_utf8(printed.stdout).unwrap();
    let stderr = String::from_utf8(printed.stderr).unwrap();
    let folder_link = link_id(stdout.trim_end());
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(
        stderr.contains("browse and download") && stderr.contains("for 2 hours"),
        "{stderr}"
    );
    assert!(!stderr.contains(&folder_link), "{stderr}");
    assert_eq!(shared(&folder_link).await.1["kind"], "NODE_KIND_DIR");

    let mut protected = machine
        .command(&[
            "share",
            "create",
            plan.to_str().unwrap(),
            "--password",
            "--expires",
            "never",
            "--json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(protected.stdin.as_mut().unwrap(), b"correct horse\n").unwrap();
    let protected = protected.wait_with_output().unwrap();
    assert!(protected.status.success(), "{protected:?}");
    let protected: Value = serde_json::from_slice(&protected.stdout).unwrap();
    schema::check(&protected);
    assert_eq!(protected["data"]["has_password"], true, "{protected:#}");
    assert_eq!(
        protected["data"]["expires_at_ms"],
        Value::Null,
        "{protected:#}"
    );
    let password_link = link_id(protected["data"]["url"].as_str().unwrap());
    let (status, info) = shared(&password_link).await;
    assert_eq!(
        (status, &info["passwordRequired"]),
        (401, &Value::Bool(true)),
        "{info:#}"
    );

    let away_link = (owner.api())
        .create_share_link(
            &owner,
            &elsewhere.space_id,
            &CreateShareLinkRequest {
                node_id: away,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .link_id;

    let secrets = [&file_link, &folder_link, &password_link, &away_link];
    let listed = machine.envelope(&["share", "list"]);
    let links = listed["data"]["links"].as_array().unwrap();
    assert_eq!(links.len(), 4, "{listed:#}");
    let away_listed = links
        .iter()
        .find(|link| link["space_id"] == elsewhere.space_id.as_str())
        .unwrap();
    assert_eq!(away_listed["path"], "away.txt", "{listed:#}");
    assert_eq!(away_listed["folder"], Value::Null, "{listed:#}");
    let table = machine.run(&["share", "list"]);
    for text in [listed.to_string(), table.clone()] {
        assert!(
            secrets.iter().all(|secret| !text.contains(secret.as_str())),
            "{text}"
        );
    }
    assert!(
        table.contains(&format!("{}/Docs/plan.txt", folder.display())),
        "{table}"
    );
    assert!(
        table.contains("away.txt in the space share elsewhere"),
        "{table}"
    );

    let id = created["id"].as_str().unwrap();
    let revoked = machine.envelope(&["share", "revoke", id]);
    assert_eq!(revoked["outcome"], "ok", "{revoked:#}");
    assert_eq!(revoked["data"]["state"], "revoked", "{revoked:#}");
    assert_eq!(shared(&file_link).await.0, 404);
    assert_eq!(
        machine.envelope(&["share", "revoke", id])["outcome"],
        "unchanged"
    );
    machine.data(&["share", "revoke", &format!("{server}s/{folder_link}")]);
    assert_eq!(shared(&folder_link).await.0, 404);
    assert_eq!(shared(&password_link).await.0, 401);

    let active = machine.data(&["share", "list"])["links"]
        .as_array()
        .unwrap()
        .len();
    let all = machine.data(&["share", "list", "--all"]);
    let states: Vec<&str> = (all["links"].as_array().unwrap().iter())
        .map(|link| link["state"].as_str().unwrap())
        .collect();
    assert_eq!(active, 2, "{all:#}");
    assert_eq!(
        states.iter().filter(|state| **state == "revoked").count(),
        2,
        "{all:#}"
    );

    let missing = machine.fails(&[
        "share",
        "create",
        folder.join("Docs/missing.txt").to_str().unwrap(),
    ]);
    assert!(missing.contains("does not exist"), "{missing}");
    let unknown = machine.fails(&["share", "revoke", "0123456789"]);
    assert!(unknown.contains("no share link"), "{unknown}");

    let listed = machine.data(&["share", "list"]);
    let here = (listed["links"].as_array().unwrap().iter())
        .filter(|link| link["space_id"] == space.space_id.as_str())
        .count();
    for _ in here..1000 {
        (owner.api())
            .create_share_link(
                &owner,
                &space.space_id,
                &CreateShareLinkRequest {
                    node_id: docs.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let output = machine.output(&["share", "create", plan.to_str().unwrap(), "--json"]);
    let full: Value = serde_json::from_slice(&output.stdout).unwrap();
    schema::check(&full);
    assert_eq!(
        (&full["error"]["code"], &full["error"]["reason"]),
        (&Value::from("failed"), &Value::from("share_link_limit")),
        "{full:#}"
    );

    assert!(stop(daemon).success());
    let logged = fs::read_to_string(&log).unwrap();
    assert!(logged.contains("DEBUG"), "the daemon logged at debug level");
    for secret in secrets
        .iter()
        .map(|secret| secret.as_str())
        .chain(["correct horse"])
    {
        assert!(
            !logged.contains(secret),
            "the daemon's log holds a link's secret"
        );
    }
    refuses_local_replacements_and_edits_that_have_not_synced().await;
}

async fn refuses_local_replacements_and_edits_that_have_not_synced() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let space_name = format!("share local {}", uuidv7());
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
    let machine = Machine::new();
    let server = server_url().to_string();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let folder = machine.path("Sync");
    fs::create_dir_all(&folder).unwrap();
    let folder = folder.canonicalize().unwrap();
    machine.run(&[
        "folder",
        "link",
        folder.to_str().unwrap(),
        "--space",
        &space_name,
    ]);
    let daemon = machine
        .command(&["_daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    eventually("the daemon's socket", || machine.socket().exists()).await;
    machine
        .status_until("the first sync", |status| {
            status["folders"][0]["state"] == "up_to_date"
        })
        .await;
    let docs = uuidv7();
    let original = uuidv7();
    commit_as_owner(
        &owner,
        &space.space_id,
        &[b"private original"],
        vec![
            op::Op::CreateDir(CreateDir {
                node_id: docs.clone(),
                parent_id: space.space_id.clone(),
                name: "Docs".into(),
                pending_for: String::new(),
            }),
            op::Op::CreateFile(CreateFile {
                made_over: Vec::new(),
                source_version_id: String::new(),
                node_id: original.clone(),
                parent_id: docs,
                name: "contract.txt".into(),
                content: Some(file_content(b"private original")),
                ..Default::default()
            }),
        ],
    )
    .await;
    let file = folder.join("Docs/contract.txt");
    eventually("the original on disk", || {
        fs::read(&file).ok().as_deref() == Some(b"private original")
    })
    .await;
    assert!(stop(daemon).success());

    let share = |path: &Path| machine.data(&["share", "create", path.to_str().unwrap()]);
    let synced = share(&file);
    assert_eq!(synced["node_id"], original);
    machine.data(&["share", "revoke", synced["id"].as_str().unwrap()]);
    let private = folder.join("Docs/contract-private.txt");
    fs::rename(&file, &private).unwrap();
    fs::write(&file, b"public replacement").unwrap();
    for path in [&file, &folder.join("Docs"), &folder] {
        let error = machine.fails(&["share", "create", path.to_str().unwrap()]);
        assert!(error.contains("has not synced yet"), "{error}");
    }
    fs::remove_file(&file).unwrap();
    fs::rename(&private, &file).unwrap();
    fs::write(&file, b"public edited in place").unwrap();
    for path in [&file, &folder.join("Docs"), &folder] {
        let error = machine.fails(&["share", "create", path.to_str().unwrap()]);
        assert!(error.contains("has not synced yet"), "{error}");
    }
    fs::write(&file, b"private original").unwrap();
    let restored = share(&file);
    assert_eq!(restored["node_id"], original);
    machine.data(&["share", "revoke", restored["id"].as_str().unwrap()]);

    fs::rename(&file, &private).unwrap();
    fs::write(&file, b"public replacement").unwrap();
    let daemon = machine
        .command(&["_daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    eventually("the restarted daemon's socket", || {
        machine.socket().exists()
    })
    .await;
    let started = Instant::now();
    let created = loop {
        let output = machine.output(&["share", "create", file.to_str().unwrap(), "--json"]);
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        schema::check(&envelope);
        if output.status.success() {
            break envelope["data"].clone();
        }
        assert!(
            started.elapsed() < WAIT,
            "replacement did not finish syncing"
        );
        assert_eq!(envelope["error"]["code"], "usage");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_ne!(created["node_id"], original);
    let id = link_id(created["url"].as_str().unwrap());
    let node = created["node_id"].as_str().unwrap();
    let reply = reqwest::get(
        server_url()
            .join(&format!("v1/share/{id}/file/{node}"))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(reply.status().as_u16(), 200);
    assert_eq!(reply.bytes().await.unwrap().as_ref(), b"public replacement");
    machine.data(&["share", "revoke", created["id"].as_str().unwrap()]);
    assert!(stop(daemon).success());
}
