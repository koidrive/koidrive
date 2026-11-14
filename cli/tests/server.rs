// SPDX-License-Identifier: Apache-2.0

//! The `koi` command against the real Worker in `server/`, run by `cli/scripts/integration-test.sh`, which starts
//! `wrangler dev` and sets `KOIDRIVE_TEST_SERVER` (its URL) and `KOIDRIVE_TEST_SERVER_LOG` (its output, where the
//! Worker prints the claim code). The owner's device is driven through `koidrive-net`; the device under test only
//! through the command.

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use koidrive_chunk::{ChunkerConfig, chunk_hash};
use koidrive_net::{Api, Auth, Credentials, Event, Session, SessionConfig, Transfer};
use koidrive_proto::v1::{
    ChunkRef, Commit, Content, CreateDir, CreateFile, CreateShareLinkRequest, CreateSpaceRequest,
    Delete, Op, PairCompleteRequest, TokenRequest, UpdateContent, op, op_result,
};
use serde_json::Value;
use tokio::sync::OnceCell;
use url::Url;

mod schema;

const WAIT: Duration = Duration::from_secs(30);

fn server_url() -> Url {
    std::env::var("KOIDRIVE_TEST_SERVER")
        .expect("KOIDRIVE_TEST_SERVER is not set; run cli/scripts/integration-test.sh, which starts the Worker")
        .parse()
        .expect("KOIDRIVE_TEST_SERVER is a URL")
}

fn api() -> Api {
    Api::local_development(server_url()).unwrap()
}

static OWNER: OnceCell<Credentials> = OnceCell::const_new();

/// The deployment's owner, claimed once for every test in this binary.
async fn owner() -> Credentials {
    OWNER.get_or_init(claim).await.clone()
}

/// Claims the deployment with `koi server claim` and the code the Worker prints to its log.
async fn claim() -> Credentials {
    let log =
        std::env::var("KOIDRIVE_TEST_SERVER_LOG").expect("KOIDRIVE_TEST_SERVER_LOG is not set");
    let setup = reqwest::get(server_url().join("setup").unwrap())
        .await
        .unwrap();
    assert!(
        setup.status().is_success(),
        "GET /setup answered {}",
        setup.status()
    );
    let started = Instant::now();
    let code = loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if let Some(code) = text.lines().rev().find_map(claim_code) {
            break code;
        }
        assert!(started.elapsed() < WAIT, "no claim code in {log}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let machine = Machine::new();
    let claimed = machine.run(&[
        "server",
        "claim",
        server_url().as_str(),
        &code,
        "--user",
        "Owner",
        "--name",
        "owner",
    ]);
    assert!(claimed.starts_with("Paired as owner"), "{claimed}");
    koidrive_driver::Credential::load(&machine.path("state/koidrive/credential.json"))
        .unwrap()
        .expect("the claim saved a credential")
        .net()
}

/// The claim code on a Worker log line that mentions one, such as `ABCD-EFGH-JKLM`.
fn claim_code(line: &str) -> Option<String> {
    if !line.to_ascii_lowercase().contains("claim code") {
        return None;
    }
    line.split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .rfind(|word| {
            word.contains('-')
                && word.len() >= 8
                && word.split('-').all(|group| {
                    !group.is_empty() && group.chars().all(|c| c.is_ascii_alphanumeric())
                })
        })
        .map(str::to_owned)
}

fn uuidv7() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let bits = (ms << 80) | (fastrand::u128(..) & ((1 << 80) - 1));
    let bits = (bits & !(0xf << 76)) | (0x7 << 76);
    let bits = (bits & !(0x3 << 62)) | (0x2 << 62);
    let hex = format!("{bits:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// A machine's home for the device under test, with its XDG directories. Its config.toml turns the background service
/// off, so that the tests run the daemon themselves and never touch the service manager of the machine running them.
struct Machine {
    home: tempfile::TempDir,
    /// The background service is on, and the device uses the machine's own runtime directory, where its service
    /// manager listens.
    service: bool,
}

impl Machine {
    fn new() -> Self {
        let machine = Machine {
            home: tempfile::tempdir().unwrap(),
            service: false,
        };
        machine.configure("");
        machine
    }

    #[cfg(target_os = "linux")]
    fn with_service() -> Self {
        Machine {
            home: tempfile::tempdir().unwrap(),
            service: true,
        }
    }

    /// Writes config.toml: `rules`, and the switch that keeps the background service off.
    fn configure(&self, rules: &str) {
        fs::create_dir_all(self.path("config/koidrive")).unwrap();
        fs::write(
            self.path("config/koidrive/config.toml"),
            format!("[service]\nbackground = false\n\n{rules}"),
        )
        .unwrap();
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn socket(&self) -> PathBuf {
        let vars = [
            ("XDG_CONFIG_HOME", self.path("config")),
            ("XDG_STATE_HOME", self.path("state")),
            ("XDG_RUNTIME_DIR", self.path("run")),
        ];
        koidrive_driver::Paths::from_vars(|name| {
            (vars.iter())
                .find(|(key, _)| *key == name)
                .map(|(_, path)| path.clone().into())
        })
        .unwrap()
        .socket()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
        command
            .args(args)
            .current_dir(self.home.path())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env_remove("KOIDRIVE_LOG")
            .env("KOIDRIVE_UPDATE_CHECK", "0");
        if !self.service {
            command.env("XDG_RUNTIME_DIR", self.path("run"));
        }
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run koi")
    }

    fn run(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "koi {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fails(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(!output.status.success(), "koi {args:?} succeeded");
        String::from_utf8(output.stderr).unwrap()
    }

    fn status(&self) -> Value {
        self.data(&["status"])
    }

    /// The data of the command's JSON envelope, after checking that the command succeeded.
    fn data(&self, args: &[&str]) -> Value {
        self.envelope(args)["data"].clone()
    }

    fn envelope(&self, args: &[&str]) -> Value {
        let args: Vec<&str> = args.iter().copied().chain(["--json"]).collect();
        let envelope: Value = serde_json::from_str(&self.run(&args)).unwrap();
        schema::check(&envelope);
        assert_eq!(envelope["schema_version"], 1, "{envelope:#}");
        assert_eq!(envelope["error"], Value::Null, "{envelope:#}");
        envelope
    }

    /// Every entry of the profile's directories with its modification time and, for a file, its contents.
    fn profile(&self) -> Vec<(PathBuf, SystemTime, Vec<u8>)> {
        let mut found = Vec::new();
        let mut todo: Vec<PathBuf> = ["config", "state", "run"]
            .iter()
            .map(|dir| self.path(dir))
            .filter(|dir| dir.exists())
            .collect();
        while let Some(dir) = todo.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                let bytes = if meta.is_dir() {
                    todo.push(path.clone());
                    Vec::new()
                } else if meta.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    Vec::new()
                };
                found.push((path, meta.modified().unwrap(), bytes));
            }
        }
        found.sort();
        found
    }

    /// Polls `koi status --json` until `done` accepts it.
    async fn status_until(&self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let started = Instant::now();
        loop {
            let status = self.status();
            if done(&status) {
                return status;
            }
            assert!(
                started.elapsed() < WAIT,
                "{what} did not happen; last status: {status:#}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

fn stop(mut daemon: Child) -> std::process::ExitStatus {
    let killed = Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let started = Instant::now();
    loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            return status;
        }
        assert!(
            started.elapsed() < WAIT,
            "the daemon did not stop after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

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
        })),
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

/// A one-chunk file of these bytes.
fn file_content(bytes: &[u8]) -> Content {
    let chunk = ChunkRef {
        hash: chunk_hash(bytes).to_vec(),
        size: bytes.len() as u32,
        encoding: 0,
    };
    koidrive_chunk::content(
        vec![chunk],
        1_700_000_000_000_000_000,
        false,
        &ChunkerConfig::V0,
    )
}

/// Uploads `files` and commits `ops` from the owner's device, as the daemon does not upload yet; returns the
/// commit's seq.
async fn commit_as_owner(
    owner: &Arc<Auth>,
    space_id: &str,
    files: &[&'static [u8]],
    ops: Vec<op::Op>,
) -> u64 {
    let config = SessionConfig::new(space_id, "cli integration test", "0", 1);
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
    let chunks: Vec<ChunkRef> = files
        .iter()
        .flat_map(|bytes| file_content(bytes).chunks)
        .collect();
    let receipts = Transfer::new(owner.api())
        .upload(&session, &chunks, |chunk: &ChunkRef| {
            let bytes = files
                .iter()
                .find(|bytes| chunk_hash(bytes)[..] == chunk.hash[..])
                .copied()
                .unwrap();
            async move { Ok(Bytes::from_static(bytes)) }
        })
        .await
        .unwrap();
    let committed = session
        .commit(Commit {
            commit_id: uuidv7(),
            base_seq: welcome.head_seq,
            ops: ops.into_iter().map(|op| Op { op: Some(op) }).collect(),
            receipts,
            history_id: welcome.history_id.clone(),
            device_seq: welcome.device_seq + 1,
            ..Default::default()
        })
        .await
        .unwrap();
    session.close().await;
    assert!(
        committed
            .results
            .iter()
            .all(|result| result.code == op_result::Code::Applied as i32),
        "{committed:?}"
    );
    committed.seq
}

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
            }),
            op::Op::CreateFile(CreateFile {
                node_id: note.clone(),
                parent_id: docs.clone(),
                name: "note.txt".into(),
                content: Some(file_content(b"first")),
            }),
            op::Op::CreateFile(CreateFile {
                node_id: gone.clone(),
                parent_id: docs.clone(),
                name: "gone.txt".into(),
                content: Some(file_content(b"deleted later")),
            }),
            op::Op::CreateFile(CreateFile {
                node_id: uuidv7(),
                parent_id: docs.clone(),
                name: "link.txt".into(),
                content: Some(file_content(b"link")),
            }),
            op::Op::CreateFile(CreateFile {
                node_id: uuidv7(),
                parent_id: docs.clone(),
                name: "target.txt".into(),
                content: Some(file_content(b"target text")),
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
                node_id: note,
                content: Some(file_content(b"second!")),
                resend: false,
            }),
            op::Op::Delete(Delete {
                node_id: gone,
                requires: None,
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
    assert_eq!(size_of_link("a link to target.txt"), 4);
    fs::remove_file(&link).unwrap();
    let elsewhere = machine.path("elsewhere.txt");
    fs::write(&elsewhere, b"not synced").unwrap();
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    assert_eq!(size_of_link("a link outside the folder"), 4);
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

async fn eventually(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < WAIT, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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
            }),
            op::Op::CreateFile(CreateFile {
                node_id: note.clone(),
                parent_id: docs.clone(),
                name: "note.txt".into(),
                content: Some(file_content(b"first")),
            }),
            op::Op::CreateFile(CreateFile {
                node_id: gone.clone(),
                parent_id: docs,
                name: "gone.txt".into(),
                content: Some(file_content(b"deleted later")),
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
                node_id: note,
                content: Some(file_content(b"second!")),
                resend: false,
            }),
            op::Op::Delete(Delete {
                node_id: gone,
                requires: None,
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

/// Needs a systemd user instance, which the CI job starts by turning lingering on, and `KOIDRIVE_TEST_SYSTEMD=1`.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker and a systemd user instance; run cli/scripts/integration-test.sh"]
async fn a_linked_folder_syncs_in_the_systemd_user_service_until_the_last_unlink() {
    if std::env::var_os("KOIDRIVE_TEST_SYSTEMD").is_none() {
        eprintln!("skipped: set KOIDRIVE_TEST_SYSTEMD=1 where systemctl --user works");
        return;
    }
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
            }),
            op::Op::CreateFile(CreateFile {
                node_id: uuidv7(),
                parent_id: docs.clone(),
                name: "plan.txt".into(),
                content: Some(file_content(b"the plan")),
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
            node_id: away.clone(),
            parent_id: elsewhere.space_id.clone(),
            name: "away.txt".into(),
            content: Some(file_content(b"away")),
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
            }),
            op::Op::CreateFile(CreateFile {
                node_id: original.clone(),
                parent_id: docs,
                name: "contract.txt".into(),
                content: Some(file_content(b"private original")),
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
            }),
            op::Op::CreateFile(CreateFile {
                node_id: early.clone(),
                parent_id: docs.clone(),
                name: "early.txt".into(),
                content: Some(file_content(b"early")),
            }),
        ],
    )
    .await;
    let delete = op::Op::Delete(Delete {
        node_id: early.clone(),
        requires: None,
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
