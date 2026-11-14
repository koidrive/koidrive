// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;
use crate::core::SessionListener;
use crate::types::{
    DeletedItem, FolderPath, NodeKind, QuestionKind, RestoreRefusal, RestoreState, RestoreTarget,
    SyncProgress,
};

const KOI: &str = r#"#!/bin/sh
dir="$(dirname "$0")"
shift
printf '%s\n' "$*" >> "$dir/calls"
printf '%s %s %s\n' "$HOME" "$XDG_STATE_HOME" "${XDG_RUNTIME_DIR-unset}" > "$dir/env"
case "$1-$2" in
  join-*|server-claim) cat > "$dir/stdin-$1" ;;
esac
if [ "$1" = status ] && [ -e "$dir/snapshot-next-status" ] && mv "$dir/snapshot-next-status" "$dir/snapshot-status-taken" 2>/dev/null; then
  cat "$dir/status.json" > "$dir/status-snapshot.json"
  touch "$dir/status-snapshot-ready"
  while [ -e "$dir/hold-status-snapshot" ]; do sleep 0.01; done
  cat "$dir/status-snapshot.json"
  exit 0
fi
while [ -e "$dir/hold-$1" ]; do sleep 0.01; done
for name in "$1-$2" "$1"; do
  if [ -f "$dir/$name.json" ]; then exec cat "$dir/$name.json"; fi
done
exit 3
"#;

const THIS: &str = "0190b6a5-0000-7000-8000-00000000000a";

const OTHER: &str = "0190b6a5-0000-7000-8000-00000000000b";

const PROMPT: Duration = Duration::from_secs(1);

const WAIT: Duration = Duration::from_secs(10);

struct Ignore;

impl SessionListener for Ignore {
    fn on_event(&self, _: SessionEvent) {}
}

struct Koi {
    dir: tempfile::TempDir,
    core: Arc<DaemonCore>,
    _listener: Arc<Box<dyn SessionListener>>,
}

impl Koi {
    fn new(status: Value) -> Self {
        Self::with_listener(status, Box::new(Ignore))
    }

    fn with_listener(status: Value, listener: Box<dyn SessionListener>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let koi = dir.path().join("koi");
        fs::write(&koi, KOI).unwrap();
        fs::set_permissions(&koi, fs::Permissions::from_mode(0o755)).unwrap();
        write(dir.path(), "status", ok(status));
        write(dir.path(), "_reconcile", ok(json!({})));
        let (home, state) = (dir.path().join("home"), dir.path().join("state"));
        let env = vec![
            ("HOME", Some(home.into_os_string())),
            ("XDG_STATE_HOME", Some(state.into_os_string())),
            ("XDG_RUNTIME_DIR", None),
        ];
        let paths = driver::Paths::from_vars(|name| {
            let (_, value) = env.iter().find(|(set, _)| *set == name)?;
            value.clone()
        })
        .unwrap();
        let listener = Arc::new(listener);
        let core = DaemonCore::open(koi, Profile { paths, env }, Events::new(&listener)).unwrap();
        Koi {
            dir,
            core,
            _listener: listener,
        }
    }

    fn answer(&self, command: &str, envelope: Value) {
        write(self.dir.path(), command, envelope);
    }

    fn calls(&self) -> Vec<String> {
        let calls = fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default();
        calls.lines().map(str::to_owned).collect()
    }

    fn calls_of(&self, command: &str) -> Vec<String> {
        (self.calls().into_iter())
            .filter(|call| call.starts_with(command))
            .collect()
    }

    fn called(&self, call: &str) {
        let started = Instant::now();
        while !self.calls().iter().any(|made| made == call) {
            assert!(
                started.elapsed() < WAIT,
                "koi never ran {call}: {:?}",
                self.calls()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn hold(&self, command: &str) -> PathBuf {
        let hold = self.dir.path().join(format!("hold-{command}"));
        fs::write(&hold, b"").unwrap();
        hold
    }
}

fn write(dir: &Path, command: &str, envelope: Value) {
    fs::write(dir.join(format!("{command}.json")), envelope.to_string()).unwrap();
}

fn ok(data: Value) -> Value {
    json!({ "outcome": "ok", "data": data, "error": null })
}

fn refused(code: &str, message: &str, next: &[&str]) -> Value {
    json!({
        "outcome": "error",
        "data": null,
        "error": { "code": code, "message": message, "retryable": false },
        "next_commands": if next.is_empty() { json!([]) } else { json!([{ "argv": next }]) },
    })
}

fn signed_in(running: bool) -> Value {
    json!({
        "account": {
            "server": "https://files.example.com/",
            "user_id": "user",
            "user_name": "Ada",
            "device_id": THIS,
            "device_name": "Ada's Mac",
        },
        "running": running,
        "paused": false,
        "folders": [{
            "path": "/Users/ada/Documents",
            "space_id": "s1",
            "space_name": "Documents",
            "state": "up_to_date",
        }],
    })
}

fn eventually(what: &str, check: impl Fn() -> bool) {
    let started = Instant::now();
    while !check() {
        assert!(started.elapsed() < WAIT, "{what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn timed<T>(call: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let result = call();
    (result, started.elapsed())
}

#[test]
fn while_no_daemon_answers_the_state_is_what_koi_status_reads() {
    let koi = Koi::new(json!({ "account": null, "running": false, "folders": [] }));
    assert_eq!(koi.core.state().account, None);
    let mut status = ok(signed_in(false));
    status["decisions"] = json!([{
        "id": "q1",
        "space_id": "s1",
        "kind": "mass_delete",
        "asked_at_ms": 7,
        "counts": { "deletions": 1200, "affected_files": 1500, "space_files": 2000, "exceeded": ["deletions"] },
        "items": [{ "node_id": "n1", "path": "Archive", "files": 1200 }],
        "answers": [],
    }]);
    koi.answer("status", status);
    koi.core.reload();

    let state = koi.core.state();
    assert_eq!(state.status, SyncStatus::Stopped);
    assert_eq!(
        state.account.unwrap().server_url,
        "https://files.example.com"
    );
    assert_eq!(state.folders[0].path, "/Users/ada/Documents");
    let [question] = state.questions.as_slice() else {
        panic!("one question expected: {:?}", state.questions);
    };
    assert_eq!(question.id, "q1");
    assert!(matches!(
        question.kind,
        QuestionKind::MassDelete {
            deletions: 1200,
            ..
        }
    ));

    koi.core.start();
    assert_eq!(koi.core.state().status, SyncStatus::Stopped);
    koi.answer("status", ok(signed_in(true)));
    koi.core.reload();
    assert_eq!(koi.core.state().status, SyncStatus::Idle);
}

#[tokio::test]
async fn direct_daemon_status_preserves_typed_update_metadata() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    for update in [
        driver::Update::Unknown,
        driver::Update::Off,
        driver::Update::UpToDate {
            checked_at_ms: 1_794_528_000_123,
        },
        driver::Update::Available {
            latest: "0.2.0".into(),
            url: "https://github.com/koidrive/koidrive/releases/tag/v0.2.0".into(),
            checked_at_ms: 1_794_528_000_123,
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let expected = Status {
            version: Some("0.1.0".into()),
            update,
            ..Status::default()
        };
        let response = Response::State {
            status: expected.clone(),
            questions: Vec::new(),
        };
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            stream.read_line(&mut request).await.unwrap();
            assert_eq!(request.trim(), r#"{"request":"watch"}"#);
            let mut line = serde_json::to_vec(&response).unwrap();
            line.push(b'\n');
            stream.into_inner().write_all(&line).await.unwrap();
        });
        let (actual, questions) = daemon_state(socket).await.unwrap();
        assert_eq!(actual, expected);
        assert!(questions.is_empty());
        server.await.unwrap();
    }
}

#[test]
fn koi_runs_in_the_environment_of_the_profile() {
    let koi = Koi::new(signed_in(true));
    let env = fs::read_to_string(koi.dir.path().join("env")).unwrap();
    let dir = koi.dir.path().display();
    assert_eq!(env, format!("{dir}/home {dir}/state unset\n"));
}

#[test]
fn opening_a_session_does_not_wait_forever_for_status() {
    let koi = Koi::new(signed_in(false));
    fs::write(&koi.core.helper.path, "#!/bin/sh\nexec sleep 30\n").unwrap();
    let (opened, took) = timed(|| {
        DaemonCore::open(
            koi.core.helper.path.clone(),
            Profile {
                paths: koi.core.paths.clone(),
                env: koi.core.helper.env.clone(),
            },
            Events::new(&koi._listener),
        )
    });
    let core = opened.unwrap();
    assert_eq!(core.state().status, SyncStatus::Stopped);
    assert!(took < WAIT, "opening waited {took:?} for status");
}

#[test]
fn starting_brings_the_service_in_line_once() {
    let koi = Koi::new(signed_in(true));
    koi.core.start();
    koi.core.stop();
    koi.core.start();
    koi.called("_reconcile");
    thread::sleep(Duration::from_millis(200));
    assert_eq!(koi.calls_of("_reconcile"), ["_reconcile"]);
    assert!(koi.calls_of("service").is_empty());
}

#[test]
fn dropping_the_session_during_a_status_read_releases_the_core() {
    for close in [false, true] {
        let koi = Koi::new(signed_in(false));
        let hold = koi.hold("status");
        koi.core.start();
        eventually("the watch never read status", || {
            koi.calls_of("status").len() >= 2
        });
        let Koi {
            dir,
            core,
            _listener,
        } = koi;
        let weak = Arc::downgrade(&core);
        let (done, dropped) = std::sync::mpsc::channel();
        let dropper = thread::spawn(move || {
            if close {
                core.close();
            }
            drop(core);
            let _ = done.send(());
        });
        let finished = dropped.recv_timeout(PROMPT);
        let released = weak.upgrade().is_none();
        fs::remove_file(hold).unwrap();
        assert!(finished.is_ok(), "dropping the session waited for status");
        assert!(released, "the status worker kept the session alive");
        dropper.join().unwrap();
        drop(dir);
    }
}

#[test]
fn a_delayed_watch_snapshot_cannot_resurrect_an_account_after_reload() {
    struct Accounts(Arc<Mutex<Vec<Option<Account>>>>);

    impl SessionListener for Accounts {
        fn on_event(&self, event: SessionEvent) {
            if let SessionEvent::Account { account } = event {
                self.0.lock().unwrap().push(account);
            }
        }
    }

    let accounts = Arc::new(Mutex::new(Vec::new()));
    let koi = Koi::with_listener(signed_in(false), Box::new(Accounts(accounts.clone())));
    fs::write(koi.dir.path().join("snapshot-next-status"), b"").unwrap();
    let hold = koi.hold("status-snapshot");
    koi.core.start();
    eventually("the watch never captured its old account", || {
        koi.dir.path().join("status-snapshot-ready").is_file()
    });

    koi.answer(
        "status",
        ok(json!({ "account": null, "running": false, "folders": [] })),
    );
    koi.core.reload();
    assert_eq!(koi.core.state().account, None);
    accounts.lock().unwrap().clear();

    let config = &koi.core.paths.config_file;
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(config, "[service]\nbackground = false\n").unwrap();
    koi.core.wake.notify_one();
    fs::remove_file(hold).unwrap();
    eventually("the watch never passed its delayed result", || {
        koi.calls_of("status").len() >= 4
    });
    koi.core.stop();

    assert!(
        accounts.lock().unwrap().iter().all(Option::is_none),
        "a delayed watch resurrected the account",
    );
    assert_eq!(koi.core.state().account, None);
}

#[test]
fn the_state_and_a_pause_answer_while_a_link_waits_on_koi() {
    let koi = Koi::new(signed_in(true));
    koi.answer("pause", ok(json!({})));
    koi.answer(
        "folder-link",
        refused("unavailable", "the server does not answer", &[]),
    );
    let hold = koi.hold("folder");
    koi.core.start();
    let linker = koi.core.clone();
    let link = thread::spawn(move || linker.link_folder("s2", "/Users/ada/New"));
    koi.called("folder link --space s2 -- /Users/ada/New");

    let (state, state_took) = timed(|| koi.core.state());
    let (paused, pause_took) = timed(|| koi.core.pause());
    fs::remove_file(hold).unwrap();
    assert_eq!(state.status, SyncStatus::Idle);
    assert!(
        state_took < PROMPT,
        "state() took {state_took:?} while the link waited"
    );
    assert!(
        pause_took < PROMPT,
        "pause() took {pause_took:?} while the link waited"
    );
    paused.unwrap();
    assert_eq!(
        link.join().unwrap(),
        Err(SessionError::ServerUnreachable {
            reason: "the server does not answer".into()
        })
    );
}

#[test]
fn a_sign_out_that_koi_refuses_says_whether_to_confirm_it() {
    let koi = Koi::new(signed_in(true));
    let confirm = ["koi", "device", "leave", "--last-device"];
    koi.answer(
        "device-leave",
        refused(
            "failed",
            "this is the account's last active device",
            &confirm,
        ),
    );
    assert_eq!(koi.core.sign_out(false), Err(SessionError::LastDevice));
    koi.answer(
        "device-leave",
        refused("unavailable", "the server does not answer", &confirm),
    );
    assert_eq!(
        koi.core.sign_out(false),
        Err(SessionError::NotRevoked {
            reason: "the server does not answer".into()
        })
    );
    koi.answer(
        "device-leave",
        refused(
            "auth_required",
            "this device is not set up",
            &["koi", "setup"],
        ),
    );
    assert_eq!(koi.core.sign_out(false), Err(SessionError::NotSignedIn));
    koi.answer("device-leave", ok(json!({ "service": { "state": "off" } })));
    koi.core.sign_out(true).unwrap();
    assert_eq!(
        koi.calls_of("device"),
        [
            "device leave",
            "device leave",
            "device leave",
            "device leave --last-device"
        ]
    );
}

#[test]
fn a_sign_out_with_cleanup_left_reports_the_paths_after_refreshing_the_signed_out_account() {
    let koi = Koi::new(signed_in(false));
    koi.answer(
        "status",
        ok(json!({ "account": null, "folders": [], "questions": [], "running": false })),
    );
    koi.answer(
        "device-leave",
        ok(json!({ "not_removed": [
        { "path": "/Users/ada/Projects/.koidrive/device.json", "error": "Permission denied" },
        { "path": "/Users/ada/.local/state/koidrive/spaces/s1", "error": "Permission denied" }
    ] })),
    );

    let Err(SessionError::Failed { reason }) = koi.core.sign_out(false) else {
        panic!("sign-out must report the remaining cleanup");
    };
    assert!(reason.contains("/Users/ada/Projects/.koidrive/device.json"));
    assert!(reason.contains("/Users/ada/.local/state/koidrive/spaces/s1"));
    assert!(reason.contains("koi device leave"));
    assert_eq!(koi.core.state().account, None);
}

#[test]
fn revoking_checks_the_device_before_it_asks_koi() {
    let koi = Koi::new(signed_in(true));
    assert_eq!(
        koi.core
            .revoke_device(&format!("{OTHER}?confirm=last-device")),
        Err(SessionError::DeviceNotFound)
    );
    assert_eq!(koi.core.revoke_device(THIS), Err(SessionError::ThisDevice));
    assert!(koi.calls_of("device").is_empty());

    koi.answer(
        "device-revoke",
        json!({ "outcome": "unchanged", "data": { "device_id": OTHER } }),
    );
    assert_eq!(
        koi.core.revoke_device(OTHER),
        Err(SessionError::DeviceNotFound)
    );
    koi.answer("device-revoke", ok(json!({ "device_id": OTHER })));
    koi.core.revoke_device(OTHER).unwrap();
    assert_eq!(
        koi.calls_of("device"),
        [
            format!("device revoke -- {OTHER}"),
            format!("device revoke -- {OTHER}")
        ]
    );
}

#[test]
fn a_pairing_code_reads_as_koi_device_invite_prints_it() {
    let koi = Koi::new(signed_in(true));
    koi.answer(
        "device-invite",
        ok(json!({
            "join_argv": ["koi", "join", "K7QM-2XPD@files.example.com"],
            "expires_at_ms": 5,
        })),
    );
    assert_eq!(
        koi.core.start_pairing(),
        Ok(PairingCode {
            code: "K7QM-2XPD@files.example.com".into(),
            expires_at_ms: 5,
        })
    );
}

#[test]
fn an_answer_koi_refuses_names_what_went_wrong() {
    let koi = Koi::new(signed_in(true));
    koi.answer(
        "resolve",
        refused("stale_decision", "the decision was replaced", &[]),
    );
    assert_eq!(
        koi.core.answer("q1", Answer::Restore),
        Err(SessionError::QuestionNotFound)
    );
    assert_eq!(
        koi.core.dismiss_notice("n1"),
        Err(SessionError::NoticeNotFound)
    );
    koi.answer("resolve", refused("usage", "q1 does not take keep", &[]));
    assert_eq!(
        koi.core.answer("q1", Answer::Keep),
        Err(SessionError::AnswerNotAllowed)
    );
    koi.answer("resolve", json!({ "outcome": "unchanged", "data": {} }));
    assert_eq!(
        koi.core.answer("q1", Answer::Restore),
        Err(SessionError::QuestionNotFound)
    );
    koi.answer("resolve", ok(json!({})));
    koi.core.answer("q1", Answer::Restore).unwrap();
    assert_eq!(
        koi.calls_of("resolve"),
        [
            "resolve -- q1 restore",
            "resolve -- n1 dismiss",
            "resolve -- q1 keep",
            "resolve -- q1 restore",
            "resolve -- q1 restore"
        ]
    );
}

#[test]
fn a_pairing_code_reaches_koi_only_on_stdin() {
    let koi = Koi::new(signed_in(false));
    koi.core.lock().server = Some(api_for("https://files.example.com").unwrap());
    koi.answer("join", ok(signed_in(false)["account"].clone()));

    let account = koi
        .core
        .pair(" K7QM-2XPD@files.example.com ", "Mac")
        .unwrap();
    assert_eq!(account.device_id, THIS);
    assert_eq!(
        koi.calls_of("join"),
        ["join --server https://files.example.com/ --name Mac --code-stdin"]
    );
    assert_eq!(
        fs::read_to_string(koi.dir.path().join("stdin-join")).unwrap(),
        "K7QM-2XPD"
    );
}

#[test]
fn a_claim_code_reaches_koi_only_on_stdin() {
    let koi = Koi::new(signed_in(false));
    koi.core.lock().server = Some(api_for("https://files.example.com").unwrap());
    koi.answer("server-claim", ok(signed_in(false)["account"].clone()));

    let account = koi.core.claim(" ABCD-EFGH ", "Ada", "Mac").unwrap();
    assert_eq!(account.device_id, THIS);
    assert_eq!(
        koi.calls_of("server"),
        ["server claim --user Ada --name Mac --code-stdin -- https://files.example.com/"]
    );
    assert_eq!(
        fs::read_to_string(koi.dir.path().join("stdin-server")).unwrap(),
        "ABCD-EFGH"
    );
}

#[test]
fn a_refused_code_is_the_codes_fault() {
    let koi = Koi::new(json!({ "account": null, "running": false, "folders": [] }));
    assert_eq!(
        koi.core.pair("K7QM-2XPD", "Mac"),
        Err(SessionError::NoServer)
    );
    koi.core.lock().server = Some(api_for("https://files.example.com").unwrap());
    koi.answer(
        "join",
        refused("wrong_code", "the code is wrong or has expired", &[]),
    );
    assert_eq!(
        koi.core.pair("K7QM-2XPD@files.example.com", "Mac"),
        Err(SessionError::InvalidCode)
    );
    assert_eq!(
        koi.calls_of("join"),
        ["join --server https://files.example.com/ --name Mac --code-stdin"]
    );
}

#[test]
fn a_usage_choice_is_saved_through_koi() {
    let koi = Koi::new(signed_in(true));
    koi.answer(
        "telemetry",
        refused("failed", "the settings cannot be written", &[]),
    );
    assert_eq!(
        koi.core.set_telemetry(true),
        Err(SessionError::NotSaved {
            reason: "the settings cannot be written".into()
        })
    );
    koi.answer("telemetry", ok(json!({})));
    koi.core.set_telemetry(false).unwrap();
    assert_eq!(koi.calls_of("telemetry"), ["telemetry on", "telemetry off"]);
}

#[test]
fn a_koi_that_prints_no_result_is_named() {
    let koi = Koi::new(signed_in(true));
    let Err(SessionError::Failed { reason }) = koi.core.spaces() else {
        panic!("a failure expected");
    };
    assert!(reason.contains("printed no result"), "{reason}");
}

#[test]
fn a_sign_out_made_with_koi_reaches_the_app_while_no_daemon_runs() {
    let koi = Koi::new(signed_in(false));
    let credential = koi.core.paths.credential();
    fs::create_dir_all(credential.parent().unwrap()).unwrap();
    fs::write(&credential, b"{}").unwrap();
    koi.core.start();
    koi.called("_reconcile");

    koi.answer(
        "status",
        ok(json!({ "account": null, "running": false, "folders": [] })),
    );
    fs::remove_file(&credential).unwrap();
    eventually("the app still shows the signed-out account", || {
        koi.core.state().account.is_none()
    });
    let calls = koi.calls();
    assert!(
        calls
            .iter()
            .all(|call| call == "status" || call == "_reconcile"),
        "{calls:?}"
    );
}

#[test]
fn a_stopped_profile_retries_a_busy_read_even_without_another_file_change() {
    let koi = Koi::new(signed_in(false));
    koi.answer(
        "status",
        refused("busy", "another koi command holds the profile", &[]),
    );
    koi.core.start();
    eventually(
        "the watcher never attempted its stopped-profile read",
        || koi.calls_of("status").len() >= 2,
    );
    koi.answer(
        "status",
        ok(json!({ "account": null, "running": false, "folders": [] })),
    );
    eventually("the watcher never retried the busy profile", || {
        koi.core.state().account.is_none()
    });
    assert!(
        koi.calls()
            .iter()
            .all(|call| call == "status" || call == "_reconcile")
    );
}

#[test]
fn a_stopped_profiles_conflict_reaches_problems_through_koi_status() {
    let koi = Koi::new(signed_in(false));
    let conflict = json!({
        "type": "conflict",
        "original": { "node_id": "original", "revisions": { "content_rev": 17, "meta_rev": 23 } },
        "copy": { "node_id": "copy", "revisions": { "content_rev": 31, "meta_rev": 41 } },
        "sent_in": "commit-17",
    });
    let mut status = ok(signed_in(false));
    status["decisions"] = json!([{
        "id": "q-conflict",
        "space_id": "s1",
        "kind": "conflict",
        "conflict": conflict,
        "asked_at_ms": 7,
        "items": [
            { "node_id": "original", "path": "notes.txt", "files": 1 },
            { "node_id": "copy", "path": "notes conflict.txt", "files": 1 },
        ],
    }]);
    koi.answer("status", status);
    koi.core.reload();

    let state = koi.core.state();
    assert_eq!(state.status, SyncStatus::Stopped);
    assert!(state.questions.is_empty());
    let [problem] = state.problems.as_slice() else {
        panic!("the stopped profile's conflict must remain visible");
    };
    assert_eq!(problem.id, "q-conflict");
    assert_eq!(problem.path.as_deref(), Some("notes.txt"));
    assert!(matches!(
        &problem.kind,
        crate::types::ProblemKind::Conflict { resolve_command }
            if resolve_command == &koi.core.helper.action_command(&["resolve"])
    ));
    assert_eq!(
        koi.core.lock().questions[0].kind,
        serde_json::from_value::<driver::QuestionKind>(conflict).unwrap(),
    );
}

#[test]
fn folders_and_decisions_changed_with_koi_reach_the_app_while_no_daemon_runs() {
    let mut no_folders = signed_in(false);
    no_folders["folders"] = json!([]);
    let koi = Koi::new(no_folders);
    koi.core.start();
    koi.called("_reconcile");

    let store = koi.core.paths.store(THIS).unwrap();
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    koi.answer("status", ok(signed_in(false)));
    fs::write(&store, b"").unwrap();
    eventually("the folder linked with koi never shows", || {
        koi.core.state().folders.len() == 1
    });

    let mut asked = ok(signed_in(false));
    asked["decisions"] = json!([{
        "id": "q1",
        "space_id": "s1",
        "kind": "mass_delete",
        "asked_at_ms": 7,
        "counts": { "deletions": 1200, "affected_files": 1500, "space_files": 2000, "exceeded": ["deletions"] },
        "items": [{ "node_id": "n1", "path": "Archive", "files": 1200 }],
        "answers": [],
    }]);
    koi.answer("status", asked);
    fs::write(store.with_file_name("store.sqlite-wal"), b"changed").unwrap();
    eventually("the decision never shows", || {
        koi.core.state().questions.len() == 1
    });
}

#[test]
fn the_session_sends_the_progress_the_daemon_reports_while_it_syncs_and_none_after() {
    use std::io::{BufRead, BufReader, Write};

    struct Progresses(Arc<Mutex<Vec<Option<SyncProgress>>>>);

    impl SessionListener for Progresses {
        fn on_event(&self, event: SessionEvent) {
            if let SessionEvent::Progress { progress } = event {
                self.0.lock().unwrap().push(progress);
            }
        }
    }

    let folder = |space: &str, state: &str, progress: Value| {
        json!({
            "space_id": space,
            "space_name": space,
            "path": format!("/Users/ada/{space}"),
            "state": state,
            "remote_nodes": 3,
            "questions": 0,
            "progress": progress,
        })
    };
    let status = |folders: Vec<Value>, progress: Value| -> Status {
        let mut status = signed_in(true);
        status["folders"] = Value::Array(folders);
        status["progress"] = progress;
        serde_json::from_value(status).unwrap()
    };
    let syncing = status(
        vec![
            folder(
                "s1",
                "catching_up",
                json!({"files_to_upload": 2, "files_to_download": 1, "bytes_done": 10, "bytes_total": 100, "current": "a.txt"}),
            ),
            folder(
                "s2",
                "catching_up",
                json!({"files_to_upload": 0, "files_to_download": 1, "bytes_done": 5, "bytes_total": 50, "current": "b.txt"}),
            ),
        ],
        json!({"files_to_upload": 2, "files_to_download": 2, "bytes_done": 15, "bytes_total": 150}),
    );
    let done = status(
        vec![
            folder("s1", "up_to_date", Value::Null),
            folder("s2", "up_to_date", Value::Null),
        ],
        Value::Null,
    );

    let seen = Arc::new(Mutex::new(Vec::new()));
    let koi = Koi::with_listener(signed_in(true), Box::new(Progresses(seen.clone())));
    let socket = koi.core.paths.socket();
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (send, states) = std::sync::mpsc::channel::<Status>();
    let daemon = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut request)
            .unwrap();
        assert_eq!(request.trim(), r#"{"request":"watch"}"#);
        let mut stream = stream;
        for status in states {
            let line = serde_json::to_string(&Response::State {
                status,
                questions: Vec::new(),
            })
            .unwrap();
            writeln!(stream, "{line}").unwrap();
        }
    });
    koi.core.start();
    send.send(syncing).unwrap();
    eventually("the session sends the daemon's progress", || {
        !seen.lock().unwrap().is_empty()
    });
    let state = koi.core.state();
    assert_eq!(state.status, SyncStatus::Syncing);
    let expected = SyncProgress {
        files_to_upload: 2,
        files_to_download: 2,
        bytes_done: 15,
        bytes_total: 150,
        current: Some(FolderPath {
            space_id: "s1".into(),
            path: "a.txt".into(),
        }),
    };
    assert_eq!(state.progress, Some(expected.clone()));

    send.send(done).unwrap();
    eventually("the session ends the progress", || {
        seen.lock().unwrap().len() == 2
    });
    assert_eq!(*seen.lock().unwrap(), [Some(expected), None]);
    assert_eq!(koi.core.state().status, SyncStatus::Idle);
    koi.core.stop();
    drop(send);
    daemon.join().unwrap();
}

fn printed(command: &str, outcome: &str, data: Value) -> Value {
    let envelope = json!({
        "schema_version": 1,
        "command": command,
        "outcome": outcome,
        "data": data,
        "error": null,
        "decisions": [],
        "next_commands": [],
    });
    published(command, &envelope);
    envelope
}

fn refused_because(command: &str, reason: &str) -> Value {
    json!({
        "schema_version": 1,
        "command": command,
        "outcome": "error",
        "data": null,
        "error": { "code": "usage", "reason": reason, "message": format!("refused: {reason}"), "retryable": false },
        "decisions": [],
        "next_commands": [],
    })
}

fn published(command: &str, envelope: &Value) {
    let file = format!(
        "{}/../../docs/json-schema/v1/koi-{}.json",
        env!("CARGO_MANIFEST_DIR"),
        command.replace(' ', "-")
    );
    let schema: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<String> = (validator.iter_errors(envelope))
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "{file}: {errors:?}");
}

#[test]
fn history_reads_what_koi_prints_and_passes_the_path_after_dashes() {
    let koi = Koi::new(signed_in(false));
    let folder = "/Users/ada/Documents";
    koi.answer(
        "history-trash",
        printed(
            "history trash",
            "ok",
            json!({
                "folder": folder,
                "entries": [
                    { "node_id": "n1", "path": "Old Slides", "kind": "folder", "trash_group": "g1", "files": 12, "bytes": 84_000_000, "modified_at_ms": null, "deleted_at_ms": 9, "deleted_by_device_id": "desk", "deleted_by_device_name": "Ada's Desk" },
                    { "node_id": "n2", "path": "Old Slides.lnk", "kind": "symlink", "trash_group": "g2", "files": 1, "bytes": 4, "modified_at_ms": 7, "deleted_at_ms": null, "deleted_by_device_id": null, "deleted_by_device_name": null },
                ],
            }),
        ),
    );
    koi.answer(
        "history-versions",
        printed(
            "history versions",
            "ok",
            json!({
                "path": "Reports/Annual Report.docx",
                "node_id": "n3",
                "versions": [
                    { "version_id": "v2", "seq": 2, "size": 10, "device_id": THIS, "device_name": "Ada's Mac", "created_at_ms": 20, "current": true },
                    { "version_id": "v1", "seq": 1, "size": 8, "device_id": OTHER, "device_name": null, "created_at_ms": 10, "current": false },
                ],
            }),
        ),
    );
    koi.answer(
        "history-restore",
        printed(
            "history restore",
            "accepted",
            json!({ "request_id": "r1", "folder": folder, "path": "Old Slides" }),
        ),
    );
    let request = |id: &str, path: Value, request: Value, state: Value| json!({ "id": id, "path": path, "asked_at_ms": 5, "request": request, "state": state });
    let version =
        json!({ "type": "version", "node_id": "n3", "version_id": "v1", "saved_at_ms": 7 });
    let deleted = json!({ "type": "from_trash", "node_id": "n1", "parent_id": null, "name": null });
    koi.answer(
        "history-requests",
        printed(
            "history requests",
            "ok",
            json!({
                "folder": folder,
                "requests": [
                    request("r1", json!("Old Slides"), deleted.clone(), json!({ "type": "restored" })),
                    request("r2", json!("Old Slides"), deleted.clone(), json!({ "type": "renamed", "name": "Old Slides 2" })),
                    request("r3", json!("Reports/Annual Report.docx"), version.clone(), json!({ "type": "copied", "node_id": "n9", "name": "Annual Report (2).docx" })),
                    request("r4", Value::Null, deleted.clone(), json!({ "type": "rejected", "reason": "not_found" })),
                    request("r5", Value::Null, version.clone(), json!({ "type": "rejected", "reason": "parent_not_dir" })),
                    request("r6", Value::Null, version.clone(), json!({ "type": "rejected", "reason": "SPACE_FROZEN" })),
                    request("r7", Value::Null, deleted.clone(), json!({ "type": "does_not_fit" })),
                    request("r8", json!("Old Slides"), deleted.clone(), json!({ "type": "pending", "waits_for": "node_problem" })),
                    request("r9", json!("Old Slides"), deleted, json!({ "type": "pending", "waits_for": null })),
                ],
            }),
        ),
    );

    let trash = koi.core.trash(folder).unwrap();
    let versions = (koi.core)
        .versions(&format!("{folder}/Reports/Annual Report.docx"))
        .unwrap();
    let from_trash = (koi.core)
        .restore_deleted(&format!("{folder}/Old Slides"), "n1")
        .unwrap();
    let of_version = (koi.core)
        .restore_version(&format!("{folder}/Reports/Annual Report.docx"), "v1")
        .unwrap();
    let requests = koi
        .core
        .restore_requests(&format!("{folder}/Reports"))
        .unwrap();

    assert_eq!(
        trash,
        Trash {
            folder: folder.into(),
            items: vec![
                DeletedItem {
                    node_id: "n1".into(),
                    path: "Old Slides".into(),
                    kind: NodeKind::Folder,
                    files: 12,
                    bytes: 84_000_000,
                    modified_at_ms: None,
                    deleted_at_ms: Some(9),
                    deleted_by_device_id: Some("desk".into()),
                    deleted_by_device_name: Some("Ada's Desk".into()),
                },
                DeletedItem {
                    node_id: "n2".into(),
                    path: "Old Slides.lnk".into(),
                    kind: NodeKind::File,
                    files: 1,
                    bytes: 4,
                    modified_at_ms: Some(7),
                    deleted_at_ms: None,
                    deleted_by_device_id: None,
                    deleted_by_device_name: None,
                },
            ],
        }
    );
    let saved: Vec<_> = (versions.versions.iter())
        .map(|version| {
            (
                version.version_id.as_str(),
                version.saved_at_ms,
                version.device_name.as_deref(),
                version.current,
            )
        })
        .collect();
    assert_eq!(
        (versions.path.as_str(), versions.node_id.as_str()),
        ("Reports/Annual Report.docx", "n3")
    );
    assert_eq!(
        saved,
        [("v2", 20, Some("Ada's Mac"), true), ("v1", 10, None, false)]
    );
    assert_eq!(versions.versions[1].device_id, OTHER);
    assert_eq!(
        from_trash,
        RestoreAsked {
            request_id: "r1".into(),
            folder: folder.into(),
            path: "Old Slides".into(),
        }
    );
    assert_eq!(of_version, from_trash);
    let deleted = RestoreTarget::Deleted {
        node_id: "n1".into(),
    };
    assert_eq!(
        (
            &requests[0].path,
            requests[0].asked_at_ms,
            &requests[0].target
        ),
        (&Some("Old Slides".to_owned()), 5, &deleted)
    );
    assert_eq!(
        requests[2].target,
        RestoreTarget::Version {
            node_id: "n3".into(),
            version_id: "v1".into(),
            saved_at_ms: Some(7),
        }
    );
    assert_eq!(requests[3].path, None);
    let states: Vec<RestoreState> = requests.into_iter().map(|request| request.state).collect();
    assert_eq!(
        states,
        [
            RestoreState::Restored,
            RestoreState::RestoredAs {
                name: "Old Slides 2".into()
            },
            RestoreState::RestoredAsCopy {
                node_id: "n9".into(),
                name: "Annual Report (2).docx".into(),
            },
            RestoreState::Refused {
                reason: RestoreRefusal::Gone
            },
            RestoreState::Refused {
                reason: RestoreRefusal::DestinationNotAFolder
            },
            RestoreState::Refused {
                reason: RestoreRefusal::Other {
                    reason: "SPACE_FROZEN".into()
                },
            },
            RestoreState::TooLarge,
            RestoreState::Pending {
                held_by_problem: true
            },
            RestoreState::Pending {
                held_by_problem: false
            },
        ]
    );
    assert_eq!(
        koi.calls_of("history"),
        [
            "history trash -- /Users/ada/Documents",
            "history versions -- /Users/ada/Documents/Reports/Annual Report.docx",
            "history restore --node n1 -- /Users/ada/Documents/Old Slides",
            "history restore --version v1 -- /Users/ada/Documents/Reports/Annual Report.docx",
            "history requests -- /Users/ada/Documents/Reports",
        ]
    );
}

#[test]
fn each_reason_koi_gives_for_a_history_refusal_names_its_error() {
    let koi = Koi::new(signed_in(false));
    let path = "/Users/ada/Elsewhere/report.docx";
    let failed = |reason: &str| SessionError::Failed {
        reason: format!("refused: {reason}"),
    };
    let cases = [
        (
            "not_in_linked_folder",
            SessionError::PathNotInLinkedFolder { path: path.into() },
        ),
        ("linked_folder_itself", SessionError::PathIsLinkedFolder),
        ("not_found", SessionError::NothingAtPath),
        ("not_deleted", SessionError::NotDeleted),
        ("is_folder", SessionError::PathIsFolder),
        ("no_such_version", SessionError::VersionNotFound),
        ("already_current", SessionError::VersionIsCurrent),
        ("several_deletions", failed("several_deletions")),
        ("not_in_trash", SessionError::DeletedItemNotFound),
    ];
    for (reason, expected) in cases {
        let envelope = refused_because("history restore", reason);
        published("history restore", &envelope);
        koi.answer("history-restore", envelope);
        assert_eq!(
            koi.core.restore_deleted(path, "n1"),
            Err(expected),
            "{reason}"
        );
    }

    koi.answer(
        "history-versions",
        refused_because("history versions", "a_reason_from_later"),
    );
    assert_eq!(koi.core.versions(path), Err(failed("a_reason_from_later")));
    koi.answer(
        "history-trash",
        refused("unavailable", "the server is busy", &[]),
    );
    assert_eq!(
        koi.core.trash(path),
        Err(SessionError::ServerUnreachable {
            reason: "the server is busy".into(),
        })
    );
}

#[test]
fn a_relative_history_path_is_refused_without_running_koi() {
    let koi = Koi::new(signed_in(false));
    let relative = "Documents/report.docx";
    let refused = Err::<(), _>(SessionError::PathNotInLinkedFolder {
        path: relative.into(),
    });

    assert_eq!(koi.core.trash(relative).map(drop), refused);
    assert_eq!(koi.core.versions(relative).map(drop), refused);
    assert_eq!(koi.core.restore_deleted(relative, "n1").map(drop), refused);
    assert_eq!(koi.core.restore_version(relative, "v1").map(drop), refused);
    assert_eq!(koi.core.restore_requests(relative).map(drop), refused);
    assert!(koi.calls_of("history").is_empty(), "{:?}", koi.calls());
}

#[test]
fn a_restore_reloads_the_state_and_a_history_read_does_not() {
    let koi = Koi::new(signed_in(false));
    let folder = "/Users/ada/Documents";
    koi.answer(
        "history-requests",
        printed(
            "history requests",
            "ok",
            json!({ "folder": folder, "requests": [] }),
        ),
    );
    koi.answer(
        "history-restore",
        printed(
            "history restore",
            "accepted",
            json!({ "request_id": "r1", "folder": folder, "path": "a.txt" }),
        ),
    );
    let mut renamed = signed_in(false);
    renamed["folders"][0]["space_name"] = json!("Papers");
    koi.answer("status", ok(renamed));
    let name = || koi.core.state().folders[0].space_name.clone();

    koi.core.restore_requests(folder).unwrap();
    assert_eq!(name(), "Documents");
    koi.core
        .restore_deleted(&format!("{folder}/a.txt"), "n1")
        .unwrap();
    assert_eq!(name(), "Papers");
}
