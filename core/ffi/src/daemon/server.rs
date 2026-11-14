// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use koidrive_net::Auth;
use koidrive_store::{Store, StoredQuestion};

use super::*;
use crate::core::SessionListener;
use crate::types::{
    FolderState, MassDeleteLimit, NodeKind, Notice, NoticeKind, Problem, ProblemKind, Question,
    QuestionKind, RecoveryGroup, RestoreState, RestoreTarget,
};

const WAIT: Duration = Duration::from_secs(30);

const PROFILE_CONFIG: &str = "[service]\nbackground = false\n\n[updates]\ncheck = false\n";

#[test]
fn integration_profile_disables_background_service_and_update_checks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, PROFILE_CONFIG).unwrap();
    let config = driver::Config::load(&path).unwrap();
    assert!(!config.service.background);
    assert!(!config.updates.check);
}

fn server_url() -> String {
    std::env::var("KOIDRIVE_TEST_SERVER").expect(
        "KOIDRIVE_TEST_SERVER is not set; run core/ffi/scripts/integration-test.sh, which starts the Worker",
    )
}

fn koi() -> PathBuf {
    std::env::var_os("KOIDRIVE_TEST_KOI")
        .expect("KOIDRIVE_TEST_KOI is not set; run core/ffi/scripts/integration-test.sh, which builds koi")
        .into()
}

fn claim_code() -> String {
    let log =
        std::env::var("KOIDRIVE_TEST_SERVER_LOG").expect("KOIDRIVE_TEST_SERVER_LOG is not set");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let setup = runtime
        .block_on(reqwest::get(format!("{}/setup", server_url())))
        .unwrap();
    assert!(
        setup.status().is_success(),
        "GET /setup answered {}",
        setup.status()
    );
    let started = Instant::now();
    loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if let Some(code) = text.lines().rev().find_map(code_on) {
            return code;
        }
        assert!(started.elapsed() < WAIT, "no claim code in {log}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn code_on(line: &str) -> Option<String> {
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

#[derive(Default)]
struct Seen {
    events: Mutex<Vec<SessionEvent>>,
    changed: Condvar,
}

struct Recorder(Arc<Seen>);

impl SessionListener for Recorder {
    fn on_event(&self, event: SessionEvent) {
        self.0.events.lock().unwrap().push(event);
        self.0.changed.notify_all();
    }
}

#[derive(Debug)]
struct Mirror {
    status: SyncStatus,
    signed_in: bool,
    folders: Vec<LinkedFolder>,
    questions: Vec<Question>,
    problems: Vec<Problem>,
    notices: Vec<Notice>,
}

impl Mirror {
    fn of(opened: &SessionState, events: &[SessionEvent]) -> Self {
        let mut mirror = Mirror {
            status: opened.status,
            signed_in: opened.account.is_some(),
            folders: opened.folders.clone(),
            questions: opened.questions.clone(),
            problems: opened.problems.clone(),
            notices: opened.notices.clone(),
        };
        for event in events {
            match event {
                SessionEvent::Status { status } => mirror.status = *status,
                SessionEvent::Account { account } => mirror.signed_in = account.is_some(),
                SessionEvent::Folders { folders } => mirror.folders = folders.clone(),
                SessionEvent::Questions { questions } => mirror.questions = questions.clone(),
                SessionEvent::Problems { problems } => mirror.problems = problems.clone(),
                SessionEvent::Notices { notices } => mirror.notices = notices.clone(),
                _ => {}
            }
        }
        mirror
    }
}

struct Device {
    home: tempfile::TempDir,
    daemon: Option<Child>,
    core: Option<Arc<DaemonCore>>,
    seen: Arc<Seen>,
    listener: Option<Arc<Box<dyn SessionListener>>>,
    opened: Option<SessionState>,
}

impl Device {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config/koidrive");
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("config.toml"), PROFILE_CONFIG).unwrap();
        fs::create_dir_all(home.path().join("home")).unwrap();
        let mut device = Device {
            home,
            daemon: None,
            core: None,
            seen: Arc::default(),
            listener: None,
            opened: None,
        };
        device.start_daemon();
        device.reopen();
        device
    }

    fn env(&self) -> Vec<(&'static str, Option<OsString>)> {
        let dir = |name: &str| Some(self.path(name).into_os_string());
        vec![
            ("HOME", dir("home")),
            ("XDG_CONFIG_HOME", dir("config")),
            ("XDG_STATE_HOME", dir("state")),
            ("XDG_RUNTIME_DIR", None),
        ]
    }

    fn paths(&self) -> driver::Paths {
        let env = self.env();
        driver::Paths::from_vars(|name| {
            let (_, value) = env.iter().find(|(set, _)| *set == name)?;
            value.clone()
        })
        .unwrap()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn start_daemon(&mut self) {
        let log = fs::File::create(self.path("daemon.log")).unwrap();
        let mut command = Command::new(koi());
        command
            .arg("_daemon")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        for (name, value) in self.env() {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        self.daemon = Some(command.spawn().unwrap());
        let socket = self.paths().socket();
        let started = Instant::now();
        while UnixStream::connect(&socket).is_err() {
            let log = fs::read_to_string(self.path("daemon.log")).unwrap_or_default();
            assert!(started.elapsed() < WAIT, "the daemon did not answer: {log}");
            thread::sleep(Duration::from_millis(20));
        }
        if let Some(core) = &self.core {
            core.wake.notify_one();
        }
    }

    fn stop_daemon(&mut self) {
        let Some(mut daemon) = self.daemon.take() else {
            return;
        };
        let _ = Command::new("kill")
            .args(["-TERM", &daemon.id().to_string()])
            .status();
        daemon.wait().unwrap();
    }

    fn session(&self) -> &DaemonCore {
        self.core.as_ref().unwrap()
    }

    fn reopen(&mut self) {
        self.core = None;
        self.seen = Arc::default();
        let listener: Arc<Box<dyn SessionListener>> =
            Arc::new(Box::new(Recorder(self.seen.clone())));
        let profile = Profile {
            paths: self.paths(),
            env: self.env(),
        };
        let core = DaemonCore::open(koi(), profile, Events::new(&listener)).unwrap();
        self.seen.events.lock().unwrap().clear();
        self.opened = Some(core.state());
        self.core = Some(core);
        self.listener = Some(listener);
    }

    fn until(&self, what: &str, done: impl Fn(&Mirror) -> bool) -> Mirror {
        let started = Instant::now();
        let mut events = self.seen.events.lock().unwrap();
        loop {
            let mirror = Mirror::of(self.opened.as_ref().unwrap(), &events);
            if done(&mirror) {
                return mirror;
            }
            let left = WAIT.saturating_sub(started.elapsed());
            assert!(
                !left.is_zero(),
                "{what} did not happen; events: {events:#?}"
            );
            events = self.seen.changed.wait_timeout(events, left).unwrap().0;
        }
    }

    fn keep_credential(&self, copy: &Path) {
        fs::copy(self.paths().credential(), copy).unwrap();
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.core = None;
        self.stop_daemon();
    }
}

fn accepted(copy: &Path) -> bool {
    let credential = driver::Credential::load(copy).unwrap().unwrap();
    let auth = Auth::new(credential.api().unwrap(), credential.net());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    match runtime.block_on(auth.api().list_spaces(&auth)) {
        Ok(_) => true,
        Err(error) if error.is_unauthenticated() => false,
        Err(error) => panic!("the server did not answer: {error}"),
    }
}

fn path_str(path: &Path) -> String {
    path.to_str().unwrap().to_owned()
}

fn eventually<T>(what: &str, mut done: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(value) = done() {
            return value;
        }
        assert!(started.elapsed() < WAIT, "{what} did not happen");
        thread::sleep(Duration::from_millis(200));
    }
}

#[test]
#[ignore = "needs the Worker and koi; run core/ffi/scripts/integration-test.sh"]
fn the_app_claims_pairs_links_and_follows_the_daemon() {
    let server = server_url();

    let mut mac = Device::new();
    mac.session().start();
    mac.until("setup being needed", |m| m.status == SyncStatus::NeedsSetup);
    assert_eq!(mac.session().spaces(), Err(SessionError::NotSignedIn));
    assert_eq!(
        mac.session().claim("ABCD-EFGH", "Ada", "Mac"),
        Err(SessionError::NoServer)
    );

    let unclaimed = mac.session().connect(&format!("{server}/")).unwrap();
    assert_eq!(unclaimed.url, server.trim_end_matches('/'));
    assert!(!unclaimed.claimed);
    assert_eq!(
        mac.session().connect("files.example.com"),
        Err(SessionError::InvalidServerUrl)
    );
    let code = claim_code();
    assert_eq!(
        mac.session().claim("WRONG-CODE", "Ada", "Mac"),
        Err(SessionError::InvalidCode)
    );
    let owner = mac.session().claim(&code, "Ada", "Ada's Mac").unwrap();
    assert_eq!(owner.device_name, "Ada's Mac");
    assert_eq!(owner.user_name, "Ada");
    assert!(mac.session().connect(&server).unwrap().claimed);
    mac.until("the account arriving", |m| m.signed_in);

    let space = mac.session().create_space("Documents").unwrap();
    assert!(
        (mac.session().spaces().unwrap().iter()).any(|listed| listed.space_id == space.space_id)
    );
    let usage = mac.session().space_usage(&space.space_id).unwrap();
    assert_eq!(
        (usage.live_files, usage.live_bytes, usage.unreferenced_bytes),
        (0, 0, 0)
    );
    let folder = mac.path("Documents");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("notes.txt"), b"notes").unwrap();
    assert!(matches!(
        mac.session()
            .link_folder(&space.space_id, &path_str(&mac.path("Missing"))),
        Err(SessionError::Failed { .. })
    ));
    let linked = mac
        .session()
        .link_folder(&space.space_id, &path_str(&folder))
        .unwrap();
    assert_eq!(linked.space_name, "Documents");
    let synced = mac.until("the first sync", |m| {
        m.status == SyncStatus::Idle
            && m.folders.len() == 1
            && m.folders[0].state == FolderState::UpToDate
    });
    assert!(synced.problems.is_empty());

    let budget = folder.join("budget.xlsx");
    fs::write(&budget, b"q3 budget").unwrap();
    eventually("the budget's upload", || {
        mac.session().versions(&path_str(&budget)).ok()
    });
    fs::remove_file(&budget).unwrap();
    let trash = eventually("the budget in the trash", || {
        let trash = mac.session().trash(&path_str(&folder)).unwrap();
        trash
            .items
            .iter()
            .any(|item| item.path == "budget.xlsx")
            .then_some(trash)
    });
    assert_eq!(trash.folder, linked.path);
    let deleted = &trash.items[0];
    assert_eq!(
        (deleted.kind, deleted.files, deleted.bytes),
        (NodeKind::File, 1, 9)
    );
    assert!(deleted.modified_at_ms.is_some());
    assert!(deleted.deleted_at_ms >= deleted.modified_at_ms);
    assert_eq!(
        (
            deleted.deleted_by_device_id.as_deref(),
            deleted.deleted_by_device_name.as_deref()
        ),
        (
            Some(owner.device_id.as_str()),
            Some(owner.device_name.as_str())
        )
    );
    assert_eq!(
        mac.session()
            .restore_deleted(&path_str(&budget), "not-a-node"),
        Err(SessionError::DeletedItemNotFound)
    );
    let asked = mac
        .session()
        .restore_deleted(&path_str(&budget), &deleted.node_id)
        .unwrap();
    assert_eq!(asked.path, "budget.xlsx");
    let restored = eventually("the budget's restore", || {
        let requests = mac.session().restore_requests(&path_str(&folder)).unwrap();
        let request = requests
            .into_iter()
            .find(|request| request.id == asked.request_id)?;
        (request.state == RestoreState::Restored).then_some(request)
    });
    assert_eq!(restored.path.as_deref(), Some("budget.xlsx"));
    assert_eq!(
        restored.target,
        RestoreTarget::Deleted {
            node_id: deleted.node_id.clone()
        }
    );
    eventually("the budget back on disk", || fs::read(&budget).ok());
    assert_eq!(fs::read(&budget).unwrap(), b"q3 budget");

    let report = folder.join("report.txt");
    let saved = |count: usize| {
        eventually("a version of the report", || {
            let versions = mac.session().versions(&path_str(&report)).ok()?;
            (versions.versions.len() == count).then_some(versions)
        })
    };
    fs::write(&report, b"first draft").unwrap();
    saved(1);
    fs::write(&report, b"second draft, longer").unwrap();
    let versions = saved(2);
    let listed: Vec<(u64, bool, Option<&str>)> = (versions.versions.iter())
        .map(|version| {
            (
                version.size,
                version.current,
                version.device_name.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (20, true, Some("Ada's Mac")),
            (11, false, Some("Ada's Mac"))
        ]
    );
    assert_eq!(
        mac.session()
            .restore_version(&path_str(&report), &versions.versions[0].version_id),
        Err(SessionError::VersionIsCurrent)
    );
    assert_eq!(
        mac.session().versions(&path_str(&folder)),
        Err(SessionError::PathIsLinkedFolder)
    );
    mac.session()
        .restore_version(&path_str(&report), &versions.versions[1].version_id)
        .unwrap();
    let first_draft = RestoreTarget::Version {
        node_id: versions.node_id.clone(),
        version_id: versions.versions[1].version_id.clone(),
        saved_at_ms: Some(versions.versions[1].saved_at_ms),
    };
    let asked = mac.session().restore_requests(&path_str(&folder)).unwrap();
    assert!(
        asked.iter().any(|request| request.target == first_draft),
        "{asked:?}"
    );
    eventually("the first draft back on disk", || {
        (fs::read(&report).ok()? == b"first draft").then_some(())
    });

    mac.session().pause().unwrap();
    mac.until("the pause", |m| m.status == SyncStatus::Paused);
    mac.reopen();
    assert_eq!(mac.session().state().status, SyncStatus::Stopped);
    mac.session().start();
    let relaunched = mac.until("the pause after a relaunch", |m| {
        m.status == SyncStatus::Paused
    });
    assert!(relaunched.signed_in);
    assert_eq!(relaunched.folders.len(), 1);
    mac.session().resume().unwrap();
    mac.until("sync after resuming", |m| m.status == SyncStatus::Idle);

    let pairing = mac.session().start_pairing().unwrap();
    assert_eq!(
        pairing.code.split_whitespace().skip(1).collect::<Vec<_>>(),
        ["--server", &format!("{server}/")]
    );
    let other = Device::new();
    other.session().connect(&server).unwrap();
    assert_eq!(
        other.session().pair("AAAA-BBBB", "Laptop"),
        Err(SessionError::InvalidCode)
    );
    let paired = other.session().pair(&pairing.code, "Laptop").unwrap();
    assert_eq!(paired.user_id, owner.user_id);
    assert_ne!(paired.device_id, owner.device_id);
    let other_folder = other.path("Documents");
    fs::create_dir_all(&other_folder).unwrap();
    other
        .session()
        .link_folder(&space.space_id, &path_str(&other_folder))
        .unwrap();
    other.session().start();
    other.until("the paired device's first sync", |m| {
        m.status == SyncStatus::Idle
    });
    assert_eq!(fs::read(other_folder.join("notes.txt")).unwrap(), b"notes");
    let kept = other.path("credential");
    other.keep_credential(&kept);
    other.session().sign_out(false).unwrap();
    other.until("the paired device's sign-out", |m| !m.signed_in);
    assert!(!accepted(&kept));
    drop(other);

    let listed = |device: &Device| {
        let mut devices: Vec<(String, bool)> = (device.session().devices().unwrap().into_iter())
            .map(|device| (device.name, device.this_device))
            .collect();
        devices.sort();
        devices
    };
    assert_eq!(listed(&mac), [("Ada's Mac".to_owned(), true)]);
    let old = Device::new();
    old.session().connect(&server).unwrap();
    let pairing = mac.session().start_pairing().unwrap();
    let old_account = old.session().pair(&pairing.code, "Old Mac").unwrap();
    assert_eq!(
        listed(&mac),
        [
            ("Ada's Mac".to_owned(), true),
            ("Old Mac".to_owned(), false)
        ]
    );
    assert_eq!(
        listed(&old),
        [
            ("Ada's Mac".to_owned(), false),
            ("Old Mac".to_owned(), true)
        ]
    );
    assert_eq!(
        mac.session().revoke_device(&owner.device_id),
        Err(SessionError::ThisDevice)
    );
    let old_kept = old.path("credential");
    old.keep_credential(&old_kept);
    mac.session().revoke_device(&old_account.device_id).unwrap();
    assert!(!accepted(&old_kept));
    assert_eq!(listed(&mac), [("Ada's Mac".to_owned(), true)]);
    assert_eq!(
        mac.session().revoke_device(&old_account.device_id),
        Err(SessionError::DeviceNotFound)
    );
    drop(old);

    let held = driver::Question {
        id: "q-mass-delete".into(),
        lineage: "q-mass-delete".into(),
        recovery_cause: None,
        space_id: space.space_id.clone(),
        asked_at_ms: 1,
        kind: driver::QuestionKind::MassDelete {
            deletions: 1_200,
            affected_files: 1_200,
            space_files: 2_000,
            exceeded: vec![driver::Limit::Deletions, driver::Limit::Share],
        },
        items: vec![driver::HeldItem {
            node_id: "n1".into(),
            path: "Archive".into(),
            files: 1_200,
            device_id: None,
        }],
    };
    let noticed = driver::Question {
        id: "n-remote-mass-delete".into(),
        lineage: "n-remote-mass-delete".into(),
        recovery_cause: None,
        space_id: space.space_id.clone(),
        asked_at_ms: 2,
        kind: driver::QuestionKind::RemoteMassDelete {
            deletions: 3,
            affected_files: 40,
            space_files: 60,
            exceeded: vec![driver::Limit::Share],
        },
        items: Vec::new(),
    };
    let store = mac.paths().store(&space.space_id).unwrap();
    mac.stop_daemon();
    mac.until("the daemon leaving", |m| m.status == SyncStatus::Stopped);
    Store::open(&store)
        .unwrap()
        .transaction(|tx| {
            for question in [&held, &noticed] {
                tx.put_question(&StoredQuestion {
                    question_id: question.id.clone(),
                    asked_at_ms: question.asked_at_ms,
                    body: serde_json::to_vec(question).unwrap(),
                    answer: None,
                })?;
            }
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
    mac.start_daemon();
    let asked = mac.until("the stored question and notice", |m| {
        m.questions.len() == 1 && m.notices.len() == 1
    });
    assert_eq!(
        asked.notices[0].kind,
        NoticeKind::RemoteMassDelete {
            deletions: 3,
            affected_files: 40,
            space_files: 60,
            exceeded: vec![MassDeleteLimit::ShareOfSpace { percent: 50 }],
        }
    );
    assert_eq!(
        mac.session().dismiss_notice(&held.id),
        Err(SessionError::NoticeNotFound),
        "a question is not a notice"
    );
    assert_eq!(
        mac.session().answer(&noticed.id, Answer::Delete),
        Err(SessionError::AnswerNotAllowed)
    );
    mac.session().dismiss_notice(&noticed.id).unwrap();
    mac.until("the dismissed notice leaving", |m| m.notices.is_empty());
    assert_eq!(
        mac.session().dismiss_notice(&noticed.id),
        Err(SessionError::NoticeNotFound)
    );
    assert_eq!(
        asked.questions[0].answers,
        vec![Answer::Delete, Answer::Restore]
    );
    assert_eq!(
        mac.session().answer(&held.id, Answer::Keep),
        Err(SessionError::AnswerNotAllowed)
    );
    mac.session().answer(&held.id, Answer::Restore).unwrap();
    mac.until("the answered question leaving", |m| m.questions.is_empty());
    assert_eq!(
        mac.session().answer(&held.id, Answer::Restore),
        Err(SessionError::QuestionNotFound)
    );

    mac.stop_daemon();
    fs::remove_file(folder.join(".koidrive/space.json")).unwrap();
    mac.start_daemon();
    let halted = mac.until("the missing marker halting the folder", |m| {
        m.folders.first().map(|folder| folder.state) == Some(FolderState::Halted)
            && !m.problems.is_empty()
    });
    assert!(matches!(
        halted.problems[0].kind,
        ProblemKind::FolderHalted { .. }
    ));
    assert_eq!(
        halted.problems[0].space_id.as_deref(),
        Some(space.space_id.as_str())
    );

    mac.session().unlink_folder(&space.space_id).unwrap();
    let unlinked = mac.until("the unlink", |m| {
        m.folders.is_empty() && m.problems.is_empty()
    });
    assert_eq!(unlinked.status, SyncStatus::NeedsSetup);
    assert_eq!(fs::read(folder.join("notes.txt")).unwrap(), b"notes");
    assert_eq!(
        mac.session().unlink_folder(&space.space_id),
        Err(SessionError::FolderNotLinked)
    );

    let again = mac.path("Again");
    fs::create_dir_all(&again).unwrap();
    fs::write(again.join("kept.txt"), b"kept").unwrap();
    mac.session()
        .link_folder(&space.space_id, &path_str(&again))
        .unwrap();
    let asked = mac.until("the second link's question", |m| m.questions.len() == 1);
    let question = &asked.questions[0];
    assert!(matches!(
        question.kind,
        QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
            ..
        }
    ));
    let held: Vec<&str> = (question.items.iter())
        .map(|item| item.path.as_str())
        .collect();
    assert_eq!(held, ["kept.txt"]);
    mac.session().answer(&question.id, Answer::Publish).unwrap();
    mac.until("the second link's sync", |m| {
        m.status == SyncStatus::Idle && m.questions.is_empty()
    });
    let spare = Device::new();
    spare.session().connect(&server).unwrap();
    let pairing = mac.session().start_pairing().unwrap();
    spare.session().pair(&pairing.code, "Spare").unwrap();
    let kept = mac.path("credential");
    mac.keep_credential(&kept);
    let spare_kept = spare.path("credential");
    spare.keep_credential(&spare_kept);
    let (mac_out, spare_out) = thread::scope(|scope| {
        let spare_out = scope.spawn(|| spare.session().sign_out(false));
        (mac.session().sign_out(false), spare_out.join().unwrap())
    });
    let (last, last_kept) = match (&mac_out, &spare_out) {
        (Err(SessionError::LastDevice), Ok(())) => (&mac, &kept),
        (Ok(()), Err(SessionError::LastDevice)) => (&spare, &spare_kept),
        outcomes => {
            panic!("one of the last two devices signs out and the other must confirm: {outcomes:?}")
        }
    };
    assert!(last.session().state().account.is_some());
    assert!(accepted(last_kept));
    last.session().sign_out(true).unwrap();
    assert!(!accepted(&spare_kept));
    let signed_out = mac.until("the sign-out", |m| {
        !m.signed_in && m.folders.is_empty() && m.status == SyncStatus::NeedsSetup
    });
    assert!(signed_out.problems.is_empty());
    assert_eq!(fs::read(again.join("kept.txt")).unwrap(), b"kept");
    assert_eq!(mac.session().spaces(), Err(SessionError::NotSignedIn));
    assert!(!accepted(&kept));
    mac.reopen();
    assert_eq!(mac.opened.as_ref().unwrap().account, None);
}
