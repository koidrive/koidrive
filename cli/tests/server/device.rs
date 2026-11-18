// SPDX-License-Identifier: Apache-2.0

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_wrong_pairing_code_is_named_as_one() {
    owner().await;
    let server = server_url().to_string();
    let machine = Machine::new();
    let join = ["join", "AAAA-BBBB", "--server", &server, "--name", "desk"];
    let output = machine.output(&[&join[..], &["--json"]].concat());
    assert_eq!(output.status.code(), Some(8), "{output:?}");
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "wrong_code", "{envelope:#}");
    assert_eq!(
        envelope["error"]["message"],
        "the code is wrong or has expired"
    );
    let said = machine.fails(&join);
    assert!(said.contains("the code is wrong or has expired"), "{said}");
    assert!(!machine.path("state/koidrive/credential.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_headless_device_pairs_links_and_follows_the_change_stream() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let space_name = format!("agent work {}", uuidv7());
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

    let paired = machine.run(&[
        "join",
        &pairing.code,
        "--server",
        &server,
        "--name",
        "agent-vm",
    ]);
    assert!(paired.starts_with("Paired as agent-vm"), "{paired}");
    let credential_file = machine.path("state/koidrive/credential.json");
    assert_eq!(mode(&credential_file), 0o600);
    assert_eq!(mode(credential_file.parent().unwrap()), 0o700);
    let credential: Value =
        serde_json::from_str(&fs::read_to_string(&credential_file).unwrap()).unwrap();
    let refresh = credential["refresh_credential"]
        .as_str()
        .unwrap()
        .to_owned();
    let device_id = credential["device_id"].as_str().unwrap().to_owned();
    let again = machine.fails(&["join", "AAAA-BBBB", "--server", &server, "--name", "again"]);
    assert!(again.contains("already paired"), "{again}");

    machine.configure("[sync]\ninclude = [\"Projects/\"]\nexclude = [\"*.tmp\"]\n");
    let folder = machine.path("Sync");
    fs::create_dir_all(folder.join("Projects")).unwrap();
    fs::write(folder.join("Projects/notes.txt"), b"agent notes").unwrap();
    let folder_arg = folder.to_str().unwrap();
    let linked = machine.run(&["folder", "link", folder_arg, "--space", &space_name]);
    assert!(linked.contains(&space.space_id), "{linked}");
    let marker: Value =
        serde_json::from_str(&fs::read_to_string(folder.join(".koidrive/space.json")).unwrap())
            .unwrap();
    assert_eq!(marker["space_id"], space.space_id.as_str());
    let refused = machine.fails(&[
        "folder",
        "link",
        machine.path("state").to_str().unwrap(),
        "--space",
        &space.space_id,
    ]);
    assert!(refused.contains("Koidrive's own files"), "{refused}");

    let status = machine.status();
    assert_eq!(status["running"], false);
    assert_eq!(status["account"]["device_id"], device_id.as_str());
    assert_eq!(status["folders"][0]["state"], "stopped");

    let log_path = machine.path("daemon.log");
    let daemon = machine
        .command(&["_daemon", "--log-format", "json"])
        .stdout(Stdio::null())
        .stderr(File::create(&log_path).unwrap())
        .spawn()
        .unwrap();
    let socket = machine.socket();
    let started = Instant::now();
    while !socket.exists() {
        assert!(
            started.elapsed() < WAIT,
            "the daemon did not open {}",
            socket.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().unwrap()), 0o700);
    let second = machine.fails(&["_daemon"]);
    assert!(second.contains("another Koidrive daemon"), "{second}");

    let status = machine
        .status_until("the first sync", |status| {
            let folder = &status["folders"][0];
            folder["state"] == "up_to_date"
                && folder["last_seq"].is_u64()
                && folder["local_entries"].as_u64() >= Some(2)
        })
        .await;
    assert_eq!(status["running"], true);
    let first_seq = status["folders"][0]["last_seq"].as_u64().unwrap();
    let first_nodes = status["folders"][0]["remote_nodes"].as_u64().unwrap();

    let mut config = SessionConfig::new(&space.space_id, "cli integration test", "0", 1);
    config.backoff.initial = Duration::from_millis(50);
    let (session, mut events) = Session::start(owner.clone(), config).unwrap();
    let welcome = loop {
        match tokio::time::timeout(WAIT, events.next())
            .await
            .unwrap()
            .unwrap()
        {
            Event::Connected(welcome) => break welcome,
            Event::Stopped(reason) => panic!("the owner's session stopped: {reason}"),
            _ => {}
        }
    };
    let mkdir = Op {
        op: Some(op::Op::CreateDir(CreateDir {
            node_id: uuidv7(),
            parent_id: space.space_id.clone(),
            name: "From the owner".into(),
            pending_for: String::new(),
        })),
        ..Default::default()
    };
    let committed = session
        .commit(Commit {
            commit_id: uuidv7(),
            base_seq: welcome.head_seq,
            ops: vec![mkdir],
            history_id: welcome.history_id.clone(),
            device_seq: welcome.device_seq + 1,
            ..Default::default()
        })
        .await
        .unwrap();
    session.close().await;
    assert!(committed.seq > first_seq);
    let status = machine
        .status_until("the owner's commit arriving", |status| {
            status["folders"][0]["last_seq"].as_u64() >= Some(committed.seq)
        })
        .await;
    assert_eq!(
        status["folders"][0]["remote_nodes"].as_u64(),
        Some(first_nodes + 1)
    );
    assert_eq!(
        status["folders"][0]["history_id"],
        welcome.history_id.as_str()
    );

    let questions: Value = serde_json::from_str(&machine.run(&["resolve", "--json"])).unwrap();
    schema::check(&questions);
    assert_eq!(questions["decisions"], Value::Array(vec![]));
    assert!(
        machine
            .run(&["resolve"])
            .contains("No decisions are waiting.")
    );

    let restarted = machine.run(&["folder", "restart", "Sync"]);
    assert!(restarted.contains("again"), "{restarted}");
    machine
        .status_until("the restarted folder up to date", |status| {
            status["folders"][0]["state"] == "up_to_date"
        })
        .await;
    let restart = ["folder", "restart", "Sync", "--request-id", "restart-1"];
    let restarted = machine.envelope(&restart);
    assert_eq!(restarted["outcome"], "ok", "{restarted:#}");
    machine
        .status_until("the folder restarted by request up to date", |status| {
            status["folders"][0]["state"] == "up_to_date"
        })
        .await;
    assert_eq!(machine.envelope(&restart), restarted, "the first outcome");
    let output = machine.output(&["folder", "restart", "Sync", "--request-id", "restart-1"]);
    let replayed = machine.output(&["folder", "restart", "Sync", "--request-id", "restart-1"]);
    assert_eq!(
        (&output.status, &output.stdout),
        (&replayed.status, &replayed.stdout)
    );
    let stale = machine.output(&[
        "folder",
        "retry",
        "Sync",
        "--resend",
        "--request-id",
        "restart-1",
        "--json",
    ]);
    assert_eq!(stale.status.code(), Some(7), "{stale:?}");
    let envelope: Value = serde_json::from_slice(&stale.stdout).unwrap();
    schema::check(&envelope);
    assert_eq!(envelope["error"]["code"], "stale_decision", "{envelope:#}");
    assert_eq!(envelope["next_commands"], Value::Array(Vec::new()));

    let unlinked = machine.run(&["folder", "unlink", "Sync"]);
    assert!(unlinked.contains(&space.space_id), "{unlinked}");
    assert_eq!(machine.status()["folders"], Value::Array(vec![]));
    let again = machine.envelope(&["folder", "unlink", "Sync"]);
    assert_eq!(again["outcome"], "unchanged", "{again:#}");
    assert_eq!(again["data"]["space_id"], Value::Null, "{again:#}");
    assert!(
        !machine
            .path(&format!("state/koidrive/spaces/{}", space.space_id))
            .exists()
    );
    assert_eq!(
        fs::read(folder.join("Projects/notes.txt")).unwrap(),
        b"agent notes"
    );

    assert!(stop(daemon).success());
    assert!(!socket.exists());

    let log = fs::read_to_string(&log_path).unwrap();
    let lines: Vec<Value> = log
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|_| panic!("not a JSON log line: {line}"))
        })
        .collect();
    let messages: Vec<&str> = lines
        .iter()
        .filter_map(|line| line["fields"]["message"].as_str())
        .collect();
    for expected in [
        "the daemon is running",
        "read the space's tree",
        "unlinked a folder",
        "the daemon stopped",
    ] {
        assert!(
            messages.contains(&expected),
            "{expected:?} missing from the log: {messages:?}"
        );
    }
    assert!(
        !log.contains(&refresh),
        "the log holds the refresh credential"
    );
    assert!(
        !log.contains(&pairing.code),
        "the log holds the pairing code"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_device_revokes_a_lost_one_and_stops_syncing_when_another_revokes_it() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let machine = Machine::new();
    let unpaired = machine.fails(&["device", "list"]);
    assert!(unpaired.contains("not set up"), "{unpaired}");
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let device_id = machine.status()["account"]["device_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let lost = api()
        .pair_complete(&PairCompleteRequest {
            code: pairing.code,
            device_name: "lost laptop".into(),
            platform: "test".into(),
            request_id: String::new(),
        })
        .await
        .unwrap();

    let listed: Value = machine.data(&["device", "list"])["devices"].clone();
    let entry = |listed: &Value, id: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|device| device["device_id"] == id)
            .unwrap_or_else(|| panic!("{id} is not listed: {listed:#}"))
            .clone()
    };
    assert_eq!(entry(&listed, &device_id)["current"], true);
    assert_eq!(entry(&listed, &lost.device_id)["name"], "lost laptop");
    assert_eq!(entry(&listed, &lost.device_id)["revoked_at_ms"], 0);
    let table = machine.run(&["device", "list"]);
    assert!(table.starts_with("ID "), "{table}");
    assert!(
        table
            .lines()
            .any(|line| line.starts_with(&device_id) && line.ends_with("this device")),
        "{table}"
    );

    let invalid = machine.fails(&["device", "revoke", "../spaces"]);
    assert!(invalid.contains("not a device ID"), "{invalid}");
    let own = machine.fails(&["device", "revoke", &device_id]);
    assert!(own.contains("cannot revoke itself"), "{own}");
    let revoked = machine.run(&["device", "revoke", &lost.device_id]);
    assert!(revoked.starts_with("Revoked device"), "{revoked}");
    let again = machine.envelope(&["device", "revoke", &lost.device_id]);
    assert_eq!(again["outcome"], "unchanged", "{again:#}");
    assert_eq!(again["data"]["device_id"], lost.device_id.as_str());
    let refresh = api()
        .token(&TokenRequest {
            device_id: lost.device_id.clone(),
            refresh_credential: lost.refresh_credential.clone(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(refresh.is_unauthenticated(), "{refresh:?}");
    let listed: Value = machine.data(&["device", "list"])["devices"].clone();
    assert!(entry(&listed, &lost.device_id)["revoked_at_ms"].as_i64() > Some(0));
    let table = machine.run(&["device", "list"]);
    assert!(
        table
            .lines()
            .any(|line| line.starts_with(&lost.device_id) && line.contains("revoked")),
        "{table}"
    );

    let space_name = format!("revocation {}", uuidv7());
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
    let folder = machine.path("Sync");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("kept.txt"), b"stays on the device").unwrap();
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
    let started = Instant::now();
    while !machine.socket().exists() {
        assert!(
            started.elapsed() < WAIT,
            "the daemon did not open its socket"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    machine
        .status_until("the first sync", |status| {
            status["running"] == true && status["folders"][0]["state"] == "up_to_date"
        })
        .await;

    owner.api().revoke_device(&owner, &device_id).await.unwrap();
    let status = machine
        .status_until("the folder halting", |status| {
            status["folders"][0]["state"] == "halted"
        })
        .await;
    let problem = status["folders"][0]["problem"].as_str().unwrap();
    assert!(problem.contains("revoked"), "{problem}");
    assert_eq!(
        fs::read(folder.join("kept.txt")).unwrap(),
        b"stays on the device"
    );
    assert!(problem.contains("docs/guide/uninstall.md"), "{problem}");
    let refused = machine.fails(&["device", "list"]);
    assert!(refused.contains("docs/guide/uninstall.md"), "{refused}");
    assert!(stop(daemon).success());
    assert_eq!(
        fs::read(folder.join("kept.txt")).unwrap(),
        b"stays on the device"
    );

    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let join = ["join", &pairing.code, "--server", &server, "--name", "desk"];
    let held = machine.fails(&join);
    assert!(held.contains("already paired"), "{held}");
    for state in [
        machine.path("state/koidrive"),
        machine.path("config/koidrive"),
        folder.join(".koidrive"),
    ] {
        if state.exists() {
            fs::remove_dir_all(state).unwrap();
        }
    }
    machine.configure("");
    machine.run(&join);
    assert_ne!(machine.status()["account"]["device_id"], device_id.as_str());
    machine.run(&[
        "folder",
        "link",
        folder.to_str().unwrap(),
        "--space",
        &space_name,
    ]);
    let socket = machine.socket();
    if socket.exists() {
        fs::remove_file(&socket).unwrap();
    }
    let mut daemon = machine
        .command(&["_daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !socket.exists() {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "the daemon exited before it opened its socket"
        );
        assert!(
            started.elapsed() < WAIT,
            "the daemon did not open its socket"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    machine
        .status_until("the sync after setting up again", |status| {
            status["running"] == true && status["folders"][0]["state"] == "up_to_date"
        })
        .await;
    assert!(stop(daemon).success());
    let mut names: Vec<_> = fs::read_dir(&folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != ".koidrive")
        .collect();
    names.sort();
    assert_eq!(names, ["kept.txt"]);
    assert_eq!(
        fs::read(folder.join("kept.txt")).unwrap(),
        b"stays on the device"
    );
}

/// A credential's two slots on a profile that the server already rotated: the old credential is still current and the new
/// one is staged, as the process that holds the profile leaves them until it hears the answer (RFC 0011 section 2).
async fn rotated_on_the_server(machine: &Machine) -> String {
    let current =
        koidrive_driver::Credential::load(&machine.path("state/koidrive/credential.json"))
            .unwrap()
            .unwrap();
    let staged: String = std::iter::repeat_with(fastrand::alphanumeric)
        .take(43)
        .collect();
    let mut request = TokenRequest {
        device_id: current.device_id,
        refresh_credential: current.refresh_credential,
        rotation: true,
        ..Default::default()
    };
    let deadline = api().token(&request).await.unwrap().server_time_ms + 60_000;
    request.next_refresh_credential = staged.clone();
    request.rotate_before_ms = deadline;
    assert!(api().token(&request).await.unwrap().rotated);
    let next = serde_json::json!({ "refresh_credential": staged, "rotate_before_ms": deadline });
    let mut file = std::os::unix::fs::OpenOptionsExt::mode(
        fs::OpenOptions::new().write(true).create_new(true),
        0o600,
    )
    .open(machine.path("state/koidrive/credential.next"))
    .unwrap();
    std::io::Write::write_all(&mut file, next.to_string().as_bytes()).unwrap();
    staged
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_command_reads_the_credential_again_when_a_rotation_left_it_staged() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    let machine = Machine::new();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    rotated_on_the_server(&machine).await;
    let before = machine.profile();

    let listed: Value = machine.data(&["space", "usage"])["spaces"].clone();
    assert!(listed.is_array(), "{listed:#}");
    assert!(machine.run(&["device", "list"]).contains("desk"));
    assert_eq!(
        machine.profile(),
        before,
        "a command that only reads the credential changes none of the profile's files"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn of_two_copies_of_one_credential_only_the_one_that_rotates_it_keeps_working() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    let first = Machine::new();
    first.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let credential = first.path("state/koidrive/credential.json");
    let copy = Machine::new();
    fs::create_dir_all(copy.path("state/koidrive")).unwrap();
    fs::copy(&credential, copy.path("state/koidrive/credential.json")).unwrap();
    copy.data(&["space", "usage"]);
    let original = koidrive_driver::Credential::load(&credential)
        .unwrap()
        .unwrap()
        .refresh_credential;
    File::options()
        .write(true)
        .open(&credential)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 3600))
        .unwrap();
    let name = format!("rotation {}", uuidv7());
    owner
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
    let folder = first.path("Rotation");
    fs::create_dir_all(&folder).unwrap();

    first.run(&["folder", "link", folder.to_str().unwrap(), "--space", &name]);
    let rotated = koidrive_driver::Credential::load(&credential)
        .unwrap()
        .unwrap()
        .refresh_credential;
    assert_ne!(
        rotated, original,
        "linking the folder rotated the credential"
    );
    assert!(!first.path("state/koidrive/credential.next").exists());
    first.data(&["space", "usage"]);
    let refused = copy.fails(&["space", "usage"]);
    assert!(
        refused.contains("refused this device's credential"),
        "{refused}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_device_shows_a_pairing_code_that_adds_another() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    let first = Machine::new();
    first.run(&[
        "join",
        &pairing.code,
        "--server",
        &server,
        "--name",
        "first",
    ]);

    let shown = first.output(&["device", "invite"]);
    assert!(shown.status.success(), "{shown:?}");
    let code = String::from_utf8(shown.stdout).unwrap();
    assert_eq!(code.lines().count(), 1, "only the code: {code:?}");
    let note = String::from_utf8(shown.stderr).unwrap();
    assert!(note.contains(&format!("  koi join {code}")), "{note}");
    let args: Vec<&str> = code.split_whitespace().collect();
    let name = format!("second {}", uuidv7());
    let mut join = vec!["join"];
    join.extend(&args);
    join.extend(["--name", name.as_str()]);
    let paired = Machine::new().run(&join);
    assert!(paired.starts_with(&format!("Paired as {name}")), "{paired}");
    let devices: Value = first.data(&["device", "list"])["devices"].clone();
    assert!(
        devices
            .as_array()
            .unwrap()
            .iter()
            .any(|device| device["name"] == name.as_str()),
        "{devices:#}"
    );
    let again = Machine::new().output(&[&join[..], &["--json"]].concat());
    assert_eq!(again.status.code(), Some(8), "{again:?}");
    let refused: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(refused["error"]["code"], "wrong_code", "{refused:#}");
    let again = Machine::new().fails(&join);
    assert!(
        again.contains("the code is wrong or has expired"),
        "{again}"
    );

    let invite = ["device", "invite", "--request-id", "invite-1"];
    let traced = first
        .command(&[&invite[..], &["--json"]].concat())
        .env("KOIDRIVE_LOG", "trace")
        .output()
        .unwrap();
    assert!(traced.status.success(), "{traced:?}");
    let asked: Value = serde_json::from_slice(&traced.stdout).unwrap();
    schema::check(&asked);
    assert_eq!(asked["outcome"], "ok", "{asked:#}");
    let code = asked["data"]["join_argv"][2].as_str().unwrap();
    let logs = String::from_utf8(traced.stderr).unwrap();
    assert!(!logs.contains(code.split('@').next().unwrap()), "{logs}");
    assert_eq!(first.envelope(&invite), asked, "the first invitation");
    let shown = first.run(&invite);
    assert!(
        shown.starts_with(code.split('@').next().unwrap()),
        "{shown}"
    );
    let mut join = vec!["join"];
    let argv: Vec<&str> = (asked["data"]["join_argv"].as_array().unwrap().iter())
        .skip(2)
        .map(|arg| arg.as_str().unwrap())
        .collect();
    join.extend(&argv);
    join.extend(["--name", "third"]);
    let paired = Machine::new().run(&join);
    assert!(paired.starts_with("Paired as third"), "{paired}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_claim_of_a_claimed_server_is_refused_and_sets_nothing_up() {
    owner().await;
    let server = server_url().to_string();
    let machine = Machine::new();
    let claim = ["server", "claim", &server, "ABCD-EFGH", "--user", "Mallory"];
    let output = machine.output(&[&claim[..], &["--json"]].concat());
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "failed", "{envelope:#}");
    let refused = machine.fails(&claim);
    assert!(refused.contains("already has an owner"), "{refused}");
    let status = machine.run(&["status"]);
    assert!(status.starts_with("Device: not set up"), "{status}");
}
