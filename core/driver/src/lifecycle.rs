// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LockResult, Mutex, MutexGuard};

use koidrive_telemetry::schema::Age;
use koidrive_telemetry::{Event, Recorder};
use serde::{Deserialize, Serialize};

use crate::claim::Identity;
use crate::failures::{self, Failure, Halted};
use crate::health::{Halting, Health};
use crate::status::{FolderState, FolderStatus};

const SYNCED_STEP_SECONDS: u64 = 600;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Recovery {
    Restart,
    Root,
    Unlink,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Halt {
    problem: String,
    recovery: Recovery,
    #[serde(default, deserialize_with = "lenient")]
    why: Option<Halted>,
    #[serde(default = "koidrive_telemetry::now", deserialize_with = "since")]
    since: u64,
}

pub(crate) const UNFINISHED_UNLINK: &str =
    "unlinking the folder did not finish; unlink it again to finish";

fn unreadable(path: &Path, error: &io::Error) -> Checkpoint {
    tracing::warn!(path = %koidrive_fs::quoted(path), %error, "a folder's lifecycle record cannot be read");
    let since = (fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok())
    .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
    .map_or_else(koidrive_telemetry::now, |since| since.as_secs());
    let halt = match path.with_file_name(UNLINKING).exists() {
        true => Halt {
            problem: UNFINISHED_UNLINK.into(),
            recovery: Recovery::Unlink,
            why: Some(Halted::Unlink),
            since,
        },
        false => Halt {
            problem: format!(
                "the folder's sync record {} can't be read ({error}); run koi folder restart to check the folder and sync it again",
                koidrive_fs::quoted(path)
            ),
            recovery: Recovery::Restart,
            why: Some(Halted::Store),
            since,
        },
    };
    Checkpoint {
        halt: Some(halt),
        ..Checkpoint::default()
    }
}

const UNLINKING: &str = "unlinking";

fn lenient<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

fn since<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    Ok(lenient(deserializer)?.unwrap_or_else(koidrive_telemetry::now))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Checkpoint {
    halt: Option<Halt>,
    root: Option<Identity>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    synced: Option<u64>,
}

pub(crate) struct Lifecycle {
    path: PathBuf,
    status: Mutex<FolderStatus>,
    checkpoint: Mutex<Checkpoint>,
    pending: AtomicU64,
    telemetry: Recorder,
    unreadable: AtomicBool,
}

impl Lifecycle {
    pub(crate) fn open(store: &Path, mut status: FolderStatus, telemetry: Recorder) -> Self {
        let path = store.with_file_name("lifecycle.json");
        let read = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Checkpoint::default()),
            Err(error) => Err(error),
        };
        let unreadable_record = read.is_err();
        let checkpoint = read.unwrap_or_else(|error| unreadable(&path, &error));
        if let Some(halt) = &checkpoint.halt {
            status.state = FolderState::Halted;
            status.problem = Some(halt.problem.clone());
        }
        Self {
            path,
            status: Mutex::new(status),
            checkpoint: Mutex::new(checkpoint),
            pending: AtomicU64::new(0),
            telemetry,
            unreadable: AtomicBool::new(unreadable_record),
        }
    }

    pub(crate) fn lock(&self) -> LockResult<MutexGuard<'_, FolderStatus>> {
        self.status.lock()
    }

    pub(crate) fn needs_restart(&self) -> bool {
        self.checkpoint
            .lock()
            .unwrap()
            .halt
            .as_ref()
            .is_some_and(|halt| halt.recovery != Recovery::Root)
    }

    pub(crate) fn root(&self) -> Option<Identity> {
        self.checkpoint.lock().unwrap().root
    }

    pub(crate) fn claimed(&self, root: Identity) -> io::Result<()> {
        let mut checkpoint = self.checkpoint.lock().unwrap();
        if checkpoint
            .halt
            .as_ref()
            .is_some_and(|halt| halt.recovery == Recovery::Unlink)
        {
            return Err(io::Error::other("the folder's unlink is unfinished"));
        }
        if checkpoint.root != Some(root) || checkpoint.halt.is_some() {
            let next = Checkpoint {
                root: Some(root),
                halt: None,
                synced: checkpoint.synced,
            };
            self.save(&next)?;
            self.cleared(checkpoint.halt.as_ref());
            *checkpoint = next;
        }
        Ok(())
    }

    pub(crate) fn restart(&self) -> io::Result<()> {
        let mut checkpoint = self.checkpoint.lock().unwrap();
        if checkpoint
            .halt
            .as_ref()
            .is_some_and(|halt| halt.recovery == Recovery::Unlink)
        {
            return Ok(());
        }
        let next = Checkpoint {
            synced: checkpoint.synced,
            ..Checkpoint::default()
        };
        koidrive_state_file::remove_with_legacy_matching(
            &self.path.with_file_name(UNLINKING),
            |_| false,
        )?;
        self.save(&next)?;
        self.cleared(checkpoint.halt.as_ref());
        *checkpoint = next;
        let mut status = self.status.lock().unwrap();
        status.state = FolderState::Stopped;
        status.problem = None;
        status.wait = None;
        Ok(())
    }

    pub(crate) fn begin_unlink(&self) -> io::Result<()> {
        let mut checkpoint = self.checkpoint.lock().unwrap();
        let next = Checkpoint {
            root: checkpoint.root,
            halt: Some(Halt {
                problem: UNFINISHED_UNLINK.into(),
                recovery: Recovery::Unlink,
                why: Some(Halted::Unlink),
                since: (checkpoint.halt.as_ref())
                    .filter(|halt| halt.recovery == Recovery::Unlink)
                    .map_or_else(koidrive_telemetry::now, |halt| halt.since),
            }),
            synced: checkpoint.synced,
        };
        koidrive_state_file::write(&self.path.with_file_name(UNLINKING), b"")?;
        self.save(&next)?;
        *checkpoint = next;
        Ok(())
    }

    pub(crate) fn settle_unreadable(&self) {
        if !self.unreadable.swap(false, Ordering::Relaxed) {
            return;
        }
        let checkpoint = self.checkpoint.lock().unwrap();
        if let Some(why) = checkpoint.halt.as_ref().and_then(|halt| halt.why) {
            failures::failed(Failure::Halted(why));
        }
        if let Err(error) = self.save(&checkpoint) {
            tracing::warn!(path = %koidrive_fs::quoted(&self.path), %error, "a folder's halt could not be saved over its unreadable lifecycle record");
        }
    }

    pub(crate) fn persist(&self) -> io::Result<()> {
        let checkpoint = self.checkpoint.lock().unwrap();
        self.save(&checkpoint)?;
        if let Some(halt) = &checkpoint.halt {
            self.status.lock().unwrap().problem = Some(halt.problem.clone());
        }
        Ok(())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn halt(&self, why: Halted, problem: String, recovery: Recovery) {
        let mut checkpoint = self.checkpoint.lock().unwrap();
        if recovery != Recovery::Unlink
            && checkpoint
                .halt
                .as_ref()
                .is_some_and(|halt| halt.recovery == Recovery::Unlink)
        {
            return;
        }
        let since =
            (checkpoint.halt.as_ref()).map_or_else(koidrive_telemetry::now, |halt| halt.since);
        checkpoint.halt = Some(Halt {
            problem: problem.clone(),
            recovery,
            why: Some(why),
            since,
        });
        let problem = match self.save(&checkpoint) {
            Ok(()) => problem,
            Err(error) => format!(
                "{problem}; {} cannot save the folder's lifecycle: {error}",
                koidrive_fs::quoted(&self.path)
            ),
        };
        let mut status = self.status.lock().unwrap();
        if status.state != FolderState::Halted {
            failures::failed(Failure::Halted(why));
        }
        status.state = FolderState::Halted;
        status.problem = Some(problem);
        status.wait = None;
    }

    fn save(&self, checkpoint: &Checkpoint) -> io::Result<()> {
        let temp = self.path.with_extension("json.tmp");
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&serde_json::to_vec(checkpoint)?)?;
            file.sync_all()?;
            fs::rename(&temp, &self.path)?;
            File::open(self.path.parent().unwrap())?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    pub(crate) fn set_pending(&self, nodes: u64) {
        self.pending.store(nodes, Ordering::Relaxed);
    }

    pub(crate) fn health(&self, now: u64) -> Health {
        let up_to_date = self.status.lock().unwrap().state == FolderState::UpToDate;
        let mut checkpoint = self.checkpoint.lock().unwrap();
        if up_to_date
            && (checkpoint.synced)
                .is_none_or(|synced| now.saturating_sub(synced) >= SYNCED_STEP_SECONDS)
        {
            checkpoint.synced = Some(now);
            if let Err(error) = self.save(&checkpoint) {
                tracing::debug!(%error, "the time a folder was last up to date was not saved");
            }
        }
        Health {
            halt: (checkpoint.halt.as_ref()).map(|halt| Halting {
                why: halt.why,
                seconds: now.saturating_sub(halt.since),
            }),
            synced_ago: (checkpoint.synced).map(|synced| now.saturating_sub(synced)),
            pending: self.pending.load(Ordering::Relaxed),
        }
    }

    fn cleared(&self, halt: Option<&Halt>) {
        let Some((why, since)) = halt.and_then(|halt| Some((halt.why?, halt.since))) else {
            return;
        };
        let at = koidrive_telemetry::now();
        self.telemetry.event(Event::ProblemCleared {
            at,
            problem: Failure::Halted(why),
            lasted: Age::of(at.saturating_sub(since)),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koidrive_telemetry::Pending;
    use serde_json::json;

    #[test]
    fn unlink_authority_survives_task_halts_and_claims_in_either_order() {
        for recovery in [Recovery::Root, Recovery::Restart] {
            for claim_first in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let store = dir.path().join("store.sqlite");
                let status: FolderStatus = serde_json::from_value(serde_json::json!({
                    "space_id":"abc", "space_name":"Work", "path":"/Work",
                    "state":"stopped", "history_id":null, "last_seq":null,
                    "head_seq":null, "remote_nodes":0, "local_entries":null,
                    "questions":0, "problem":null,
                }))
                .unwrap();
                let lifecycle = Lifecycle::open(&store, status.clone(), Recorder::off());
                let identity = Identity::of(&fs::metadata(dir.path()).unwrap());
                lifecycle.claimed(identity).unwrap();
                lifecycle.halt(
                    Halted::Unlink,
                    "unlink owns recovery".into(),
                    Recovery::Unlink,
                );
                if claim_first {
                    assert!(lifecycle.claimed(identity).is_err());
                }
                lifecycle.halt(Halted::Io, "late task halt".into(), recovery);
                if !claim_first {
                    assert!(lifecycle.claimed(identity).is_err());
                }
                lifecycle.restart().unwrap();
                drop(lifecycle);
                let reopened = Lifecycle::open(&store, status, Recorder::off());
                let checkpoint = reopened.checkpoint.lock().unwrap();
                assert_eq!(checkpoint.halt.as_ref().unwrap().recovery, Recovery::Unlink);
                assert_eq!(checkpoint.root, Some(identity));
                drop(checkpoint);
                assert!(reopened.needs_restart());
                let status = reopened.lock().unwrap();
                assert_eq!(status.state, FolderState::Halted);
                assert_eq!(status.problem.as_deref(), Some("unlink owns recovery"));
            }
        }
    }

    fn status() -> FolderStatus {
        serde_json::from_value(json!({
            "space_id":"abc", "space_name":"Work", "path":"/Work",
            "state":"stopped", "history_id":null, "last_seq":null,
            "head_seq":null, "remote_nodes":0, "local_entries":null,
            "questions":0, "problem":null,
        }))
        .unwrap()
    }

    #[test]
    fn a_halt_keeps_when_it_began_while_it_repeats_and_across_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let lifecycle = Lifecycle::open(&store, status(), Recorder::off());
        lifecycle.halt(Halted::Withdrawn, "moved away".into(), Recovery::Root);
        let began = lifecycle
            .checkpoint
            .lock()
            .unwrap()
            .halt
            .as_ref()
            .unwrap()
            .since;
        lifecycle.halt(Halted::Root, "missing".into(), Recovery::Root);
        drop(lifecycle);
        let reopened = Lifecycle::open(&store, status(), Recorder::off());
        let health = reopened.health(began + 7_200);
        let halt = health.halt.unwrap();
        assert_eq!((halt.why, halt.seconds), (Some(Halted::Root), 7_200));
        assert_eq!(health.synced_ago, None);
    }

    #[test]
    fn an_up_to_date_folder_saves_when_it_was_last_synced_every_ten_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let lifecycle = Lifecycle::open(&store, status(), Recorder::off());
        lifecycle.lock().unwrap().state = FolderState::UpToDate;
        lifecycle.set_pending(5);
        assert_eq!(lifecycle.health(1_000).synced_ago, Some(0));
        assert_eq!(lifecycle.health(1_599).synced_ago, Some(599));
        assert_eq!(lifecycle.health(1_600).synced_ago, Some(0));
        lifecycle.lock().unwrap().state = FolderState::Offline;
        let health = lifecycle.health(9_000);
        assert_eq!((health.synced_ago, health.pending), (Some(7_400), 5));
        lifecycle.restart().unwrap();
        drop(lifecycle);
        let reopened = Lifecycle::open(&store, status(), Recorder::off());
        assert_eq!(reopened.health(9_000).synced_ago, Some(7_400));
    }

    #[test]
    fn a_record_with_values_this_build_cannot_read_still_opens_and_counts_as_halted() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let record = json!({
            "halt": { "problem": "x", "recovery": "Root", "why": "quota_full", "since": "yesterday" },
            "root": null,
            "synced": -5,
        });
        fs::write(store.with_file_name("lifecycle.json"), record.to_string()).unwrap();
        let lifecycle = Lifecycle::open(&store, status(), Recorder::off());
        let now = koidrive_telemetry::now();
        let health = lifecycle.health(now + 60);
        let halt = health.halt.unwrap();
        assert_eq!(halt.why, None);
        assert!((60..120).contains(&halt.seconds), "{}", halt.seconds);
        assert_eq!(health.synced_ago, None);
        assert_eq!(lifecycle.lock().unwrap().state, FolderState::Halted);
    }

    #[test]
    fn a_record_with_a_recovery_this_build_does_not_know_halts_its_folder_until_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let record = json!({ "halt": { "problem": "x", "recovery": "Purge" }, "root": null });
        fs::write(store.with_file_name("lifecycle.json"), record.to_string()).unwrap();
        let lifecycle = Lifecycle::open(&store, status(), Recorder::off());
        assert!(lifecycle.needs_restart());
        let halt = lifecycle.health(koidrive_telemetry::now()).halt.unwrap();
        assert_eq!(halt.why, Some(Halted::Store));
        let problem = lifecycle.lock().unwrap().problem.clone().unwrap();
        assert!(problem.contains("koi folder restart"), "{problem}");
        lifecycle.restart().unwrap();
        assert!(!lifecycle.needs_restart());
        drop(lifecycle);
        assert!(!Lifecycle::open(&store, status(), Recorder::off()).needs_restart());
    }

    #[test]
    fn an_unlink_whose_record_became_unreadable_stays_an_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let lifecycle = Lifecycle::open(&store, status(), Recorder::off());
        lifecycle.begin_unlink().unwrap();
        drop(lifecycle);
        fs::write(store.with_file_name("lifecycle.json"), b"{\"halt\":").unwrap();
        let reopened = Lifecycle::open(&store, status(), Recorder::off());
        reopened.restart().unwrap();
        assert!(reopened.needs_restart());
        assert!(store.with_file_name(UNLINKING).exists());
        let halt = reopened.health(koidrive_telemetry::now()).halt.unwrap();
        assert_eq!(halt.why, Some(Halted::Unlink));
        assert_eq!(
            reopened.lock().unwrap().problem.as_deref(),
            Some(UNFINISHED_UNLINK)
        );
    }

    #[test]
    fn a_restart_forgets_an_unlink_that_never_took_hold() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        fs::write(store.with_file_name(UNLINKING), b"").unwrap();
        let lifecycle = Lifecycle::open(&store, status(), Recorder::off());
        lifecycle.halt(
            Halted::Io,
            "the folder could not be unlinked".into(),
            Recovery::Restart,
        );
        lifecycle.restart().unwrap();
        assert!(!store.with_file_name(UNLINKING).exists());
        fs::write(store.with_file_name("lifecycle.json"), b"not json").unwrap();
        let reopened = Lifecycle::open(&store, status(), Recorder::off());
        let halt = reopened.health(koidrive_telemetry::now()).halt.unwrap();
        assert_eq!(halt.why, Some(Halted::Store));
    }

    #[test]
    fn an_unlink_tried_again_keeps_when_its_halt_began() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let record = json!({
            "halt": { "problem": "x", "recovery": "Unlink", "why": "unlink", "since": 1_000 },
            "root": null,
        });
        fs::write(store.with_file_name("lifecycle.json"), record.to_string()).unwrap();
        let lifecycle = Lifecycle::open(&store, status(), Recorder::off());
        lifecycle.begin_unlink().unwrap();
        drop(lifecycle);
        let reopened = Lifecycle::open(&store, status(), Recorder::off());
        let halt = reopened.health(1_000 + 7_200).halt.unwrap();
        assert_eq!((halt.why, halt.seconds), (Some(Halted::Unlink), 7_200));
    }

    #[test]
    fn a_record_from_an_older_release_still_opens_and_counts_as_halted() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let record = json!({ "halt": { "problem": "missing", "recovery": "Root" }, "root": null });
        fs::write(store.with_file_name("lifecycle.json"), record.to_string()).unwrap();
        let recorder = Recorder::on();
        let lifecycle = Lifecycle::open(&store, status(), recorder.clone());
        let halt = lifecycle.health(koidrive_telemetry::now()).halt.unwrap();
        assert_eq!(halt.why, None);
        lifecycle.restart().unwrap();
        assert_eq!(recorder.take(), Pending::default());
    }

    #[test]
    fn a_restart_or_a_claim_that_ends_a_halt_records_it_cleared_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let recorder = Recorder::on();
        let lifecycle = Lifecycle::open(&store, status(), recorder.clone());
        let identity = Identity::of(&fs::metadata(dir.path()).unwrap());
        lifecycle.claimed(identity).unwrap();
        lifecycle.halt(Halted::Claim, "another device".into(), Recovery::Restart);
        lifecycle.restart().unwrap();
        lifecycle.restart().unwrap();
        lifecycle.halt(Halted::Root, "missing".into(), Recovery::Root);
        lifecycle.claimed(identity).unwrap();
        lifecycle.claimed(identity).unwrap();
        let cleared: Vec<_> = (recorder.take().events.into_iter())
            .map(|event| serde_json::to_value(event).unwrap())
            .map(|event| (event["problem"].clone(), event["lasted"].clone()))
            .collect();
        assert_eq!(
            cleared,
            [
                (json!("halted.claim"), json!("under_1h")),
                (json!("halted.root"), json!("under_1h")),
            ]
        );
    }
}

#[cfg(test)]
mod unreachable_tests {
    use super::*;

    #[test]
    fn a_halted_or_restarted_folder_no_longer_says_it_cannot_reach_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let status: FolderStatus = serde_json::from_value(serde_json::json!({
            "space_id":"abc", "space_name":"Work", "path":"/Work",
            "state":"offline", "history_id":null, "last_seq":null,
            "head_seq":null, "remote_nodes":0, "local_entries":null,
            "questions":0, "problem":"can't connect to the server: the server's address could not be found",
            "wait": {"kind": "retry", "until_ms": 1, "unreachable": {"why": "the server's address could not be found"}},
        }))
        .unwrap();
        let lifecycle = Lifecycle::open(&store, status, Recorder::off());
        lifecycle.halt(Halted::Io, "the folder is missing".into(), Recovery::Root);
        {
            let status = lifecycle.lock().unwrap();
            assert_eq!(status.state, FolderState::Halted);
            assert_eq!(status.wait, None, "halted: {status:?}");
        }
        lifecycle.restart().unwrap();
        let status = lifecycle.lock().unwrap();
        assert_eq!(status.wait, None, "restarted: {status:?}");
    }
}
