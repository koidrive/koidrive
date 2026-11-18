// SPDX-License-Identifier: Apache-2.0

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn usage_lists_what_each_space_stores() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let machine = Machine::new();
    let unpaired = machine.fails(&["space", "usage"]);
    assert!(unpaired.contains("not set up"), "{unpaired}");
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let name = format!("usage {}", uuidv7());
    let space = owner
        .api()
        .create_space(
            &owner,
            &CreateSpaceRequest {
                name: name.clone(),
                request_id: String::new(),
            },
        )
        .await
        .unwrap();

    let listed: Value = machine.data(&["space", "usage"])["spaces"].clone();
    let entry = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["space_id"] == space.space_id.as_str())
        .unwrap_or_else(|| panic!("the new space is not listed: {listed:#}"));
    assert_eq!(entry["name"], name.as_str());
    for field in [
        "live_files",
        "live_bytes",
        "trashed_files",
        "trash_bytes",
        "old_versions",
        "old_version_bytes",
        "unreferenced_bytes",
        "total_bytes",
    ] {
        assert_eq!(entry[field], 0, "{field} of a new space: {entry:#}");
    }
    let report = machine.run(&["space", "usage"]);
    assert!(
        report.contains(&format!("{name} ({})\n  Live files ", space.space_id)),
        "{report}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_device_creates_a_space_and_links_a_folder_to_it() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    let machine = Machine::new();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let name = format!("notes {}", uuidv7());

    let created = machine.output(&["space", "create", &format!(" {name} ")]);
    assert!(created.status.success(), "{created:?}");
    let note = String::from_utf8(created.stderr).unwrap();
    assert!(
        note.starts_with(&format!("Created the space {name} (")),
        "{note}"
    );
    let created = String::from_utf8(created.stdout).unwrap();
    let spaces: Value = machine.data(&["space", "list"])["spaces"].clone();
    let ours: Vec<&Value> = spaces
        .as_array()
        .unwrap()
        .iter()
        .filter(|space| space["name"] == name.as_str())
        .collect();
    assert_eq!(ours.len(), 1, "{spaces:#}");
    let space_id = ours[0]["space_id"].as_str().unwrap();
    assert_eq!(created, format!("{space_id}\n"), "only the ID");
    let listed = machine.run(&["space", "list"]);
    assert!(listed.contains(&format!("{space_id}  {name}")), "{listed}");
    let again = machine.output(&["space", "create", &name]);
    assert!(again.status.success(), "{again:?}");
    assert_eq!(
        String::from_utf8(again.stdout).unwrap(),
        created,
        "the same ID"
    );
    let note = String::from_utf8(again.stderr).unwrap();
    assert!(
        note.starts_with(&format!("The space {name} ({space_id}) already exists.\n")),
        "{note}"
    );
    let again = machine.envelope(&["space", "create", &name]);
    assert_eq!(again["outcome"], "unchanged", "{again:#}");
    assert_eq!(again["data"]["space_id"], space_id);
    assert_eq!(
        owner
            .api()
            .list_spaces(&owner)
            .await
            .unwrap()
            .spaces
            .iter()
            .filter(|space| space.name == name)
            .count(),
        1
    );
    let blank = machine.fails(&["space", "create", "  "]);
    assert!(blank.contains("a space needs a name"), "{blank}");

    let folder = machine.path("Notes");
    fs::create_dir_all(&folder).unwrap();
    let linked = machine.run(&["folder", "link", folder.to_str().unwrap(), "--space", &name]);
    assert!(linked.contains(space_id), "{linked}");
    for space in [name.as_str(), space_id] {
        let again =
            machine.envelope(&["folder", "link", folder.to_str().unwrap(), "--space", space]);
        assert_eq!(again["outcome"], "unchanged", "{again:#}");
        assert_eq!(again["data"]["space_id"], space_id);
    }
    let other = format!("other {}", uuidv7());
    machine.run(&["space", "create", &other]);
    let refused = machine.fails(&[
        "folder",
        "link",
        folder.to_str().unwrap(),
        "--space",
        &other,
    ]);
    assert!(refused.contains("overlaps the linked folder"), "{refused}");
    let folders = machine.status()["folders"].clone();
    assert_eq!(folders.as_array().unwrap().len(), 1, "{folders:#}");
    assert_eq!(folders[0]["space_id"], space_id);
    let owners = owner.api().list_spaces(&owner).await.unwrap().spaces;
    assert!(
        owners.iter().any(|space| space.space_id == space_id),
        "the owner's other device does not see the space"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_folder_linked_with_reports_on_before_the_daemon_runs_waits_for_its_first_sync_time() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    let machine = Machine::new();
    let run = |args: &[&str]| {
        let output = machine
            .command(args)
            .env_remove("CI")
            .env_remove("DO_NOT_TRACK")
            .env_remove("KOIDRIVE_TELEMETRY")
            .env(
                "KOIDRIVE_TELEMETRY_URL",
                format!("http://{}", koidrive_test_support::reset_tcp_address()),
            )
            .output()
            .unwrap();
        assert!(output.status.success(), "koi {args:?} failed: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    run(&["telemetry", "on"]);
    let name = format!("notes {}", uuidv7());
    let space_id = run(&["space", "create", &name]).trim().to_owned();
    let folder = machine.path("Notes");
    fs::create_dir_all(&folder).unwrap();
    run(&["folder", "link", folder.to_str().unwrap(), "--space", &name]);

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(machine.path("state/koidrive/telemetry/settings.json")).unwrap(),
    )
    .unwrap();
    let install = settings["install_id"].as_str().unwrap();
    let digest: String = <sha2::Sha256 as sha2::Digest>::digest(install.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let marker =
        fs::read_to_string(machine.path(&format!("state/koidrive/spaces/{space_id}/first-sync")))
            .expect("the link left no first-sync marker for the daemon");
    let (linked, fingerprint) = marker.split_once(' ').unwrap();
    assert!(linked.parse::<i64>().is_ok(), "{marker}");
    assert_eq!(fingerprint, digest, "{marker}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn two_creates_of_one_name_at_once_make_one_space() {
    use http_body_util::{BodyExt, Full};
    use hyper::{Request, Response, body::Incoming, service::service_fn};
    use hyper_util::rt::TokioIo;
    let owner = Arc::new(Auth::new(api(), owner().await));
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let machine = Machine::new();
    machine.run(&[
        "join",
        &pairing.code,
        "--server",
        server_url().as_str(),
        "--name",
        "desk",
    ]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}/", listener.local_addr().unwrap());
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let listings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let upstream = server_url();
    let service = tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let barrier = barrier.clone();
            let listings = listings.clone();
            let upstream = upstream.clone();
            tokio::spawn(async move {
                let handler = service_fn(move |request: Request<Incoming>| {
                    let barrier = barrier.clone();
                    let listings = listings.clone();
                    let upstream = upstream.clone();
                    async move {
                        let (parts, body) = request.into_parts();
                        let path = parts.uri.path_and_query().unwrap().as_str();
                        let listing =
                            parts.method == hyper::Method::GET && parts.uri.path() == "/v1/spaces";
                        let body = body.collect().await.unwrap().to_bytes();
                        let mut call = reqwest::Client::new()
                            .request(
                                parts.method,
                                upstream.join(path.trim_start_matches('/')).unwrap(),
                            )
                            .body(body);
                        for (key, value) in &parts.headers {
                            if key != hyper::header::HOST {
                                call = call.header(key, value);
                            }
                        }
                        let response = call.send().await.unwrap();
                        let status = response.status();
                        let body = response.bytes().await.unwrap();
                        if listing && listings.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2
                        {
                            barrier.wait().await;
                        }
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(status)
                                .header("content-type", "application/x-protobuf")
                                .body(Full::new(body))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tcp), handler)
                    .await;
            });
        }
    });
    let path = machine.path("state/koidrive/credential.json");
    let mut credential: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    credential["server"] = proxy.into();
    fs::write(&path, serde_json::to_vec(&credential).unwrap()).unwrap();
    let name = format!("concurrent {}", uuidv7());
    let left = machine
        .command(&["space", "create", &name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let right = machine
        .command(&["space", "create", &name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let outcomes = tokio::task::spawn_blocking(move || {
        [
            left.wait_with_output().unwrap(),
            right.wait_with_output().unwrap(),
        ]
    })
    .await
    .unwrap();
    let spaces = owner.api().list_spaces(&owner).await.unwrap().spaces;
    let count = spaces.iter().filter(|space| space.name == name).count();
    service.abort();
    let root = machine.path("Sync");
    fs::create_dir(&root).unwrap();
    credential["server"] = server_url().to_string().into();
    fs::write(&path, serde_json::to_vec(&credential).unwrap()).unwrap();
    assert_eq!(count, 1, "both creates of one name made a space");
    let space_id = &spaces
        .iter()
        .find(|space| space.name == name)
        .unwrap()
        .space_id;
    for outcome in &outcomes {
        assert!(outcome.status.success(), "{outcome:?}");
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            format!(
                "{space_id}
"
            )
        );
    }
    let notes: Vec<String> = (outcomes.iter())
        .map(|outcome| String::from_utf8_lossy(&outcome.stderr).into_owned())
        .collect();
    assert_eq!(
        notes
            .iter()
            .filter(|note| note.starts_with("Created the space"))
            .count(),
        1,
        "{notes:?}"
    );
    let linked = machine.run(&["folder", "link", root.to_str().unwrap(), "--space", &name]);
    assert!(linked.contains(space_id.as_str()), "{linked}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_name_longer_than_255_bytes_is_refused_before_it_reaches_the_server() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    let machine = Machine::new();
    let longest = "가".repeat(85);
    let too_long = format!("{longest}a");

    let refused = machine.fails(&[
        "join",
        &pairing.code,
        "--server",
        &server,
        "--name",
        &too_long,
    ]);
    assert!(refused.contains("at most 255 bytes"), "{refused}");
    machine.run(&[
        "join",
        &pairing.code,
        "--server",
        &server,
        "--name",
        &longest,
    ]);
    let devices: Value = machine.data(&["device", "list"])["devices"].clone();
    assert!(
        devices
            .as_array()
            .unwrap()
            .iter()
            .any(|device| device["name"] == longest.as_str()),
        "{devices:#}"
    );

    let refused = machine.fails(&["space", "create", &too_long]);
    assert!(refused.contains("at most 255 bytes"), "{refused}");
    let name = format!("{}{}", uuidv7(), "가".repeat(73));
    assert_eq!(name.len(), 255);
    let created = machine.run(&["space", "create", &name]);
    let spaces: Value = machine.data(&["space", "list"])["spaces"].clone();
    assert!(
        (spaces.as_array().unwrap().iter()).any(|space| space["name"] == name.as_str()
            && format!("{}\n", space["space_id"].as_str().unwrap()) == created),
        "{created:?} {spaces:#}"
    );
}
