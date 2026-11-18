// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File};

use std::os::unix::fs::PermissionsExt;

use std::path::{Path, PathBuf};

use std::process::{Child, Command, Output, Stdio};

use std::sync::Arc;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use koidrive_test_support::Sandbox;

use koidrive_chunk::{ChunkerConfig, chunk_hash};

use koidrive_net::{Api, Auth, Credentials, Event, Session, SessionConfig, Transfer};

use koidrive_proto::v1::{
    ChunkRef, Commit, Content, CreateDir, CreateFile, CreateShareLinkRequest, CreateSpaceRequest,
    Delete, Op, PairCompleteRequest, TokenRequest, UpdateContent, op, op_result,
};

use serde_json::Value;

use tokio::sync::OnceCell;

use url::Url;

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

#[cfg(target_os = "linux")]
static SYSTEMD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    home: Sandbox,
    /// The background service is on, and the device uses the machine's own runtime directory, where its service
    /// manager listens.
    service: bool,
    koi: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let machine = Machine {
            home: Sandbox::new(),
            service: false,
            koi: PathBuf::from(process::KOI),
        };
        machine.configure("");
        machine
    }

    #[cfg(target_os = "linux")]
    fn with_service() -> Self {
        Machine {
            home: Sandbox::new(),
            service: true,
            koi: PathBuf::from(process::KOI),
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

    #[cfg(target_os = "linux")]
    fn install(&mut self) {
        let koi = self.path(".local/bin/koi");
        fixture::executable(&koi, File::open(&self.koi).unwrap());
        self.koi = koi;
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = koidrive_test_support::sandboxed(&self.koi, self.home.path());
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
            ops: ops
                .into_iter()
                .map(|op| Op {
                    op: Some(op),
                    ..Default::default()
                })
                .collect(),
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

async fn eventually(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < WAIT, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
