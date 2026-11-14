// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{LockResult, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::claim::Identity;
use crate::failures::{self, Failure, Halted};
use crate::status::{FolderState, FolderStatus};

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
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Checkpoint {
    halt: Option<Halt>,
    root: Option<Identity>,
}

pub(crate) struct Lifecycle {
    path: PathBuf,
    status: Mutex<FolderStatus>,
    checkpoint: Mutex<Checkpoint>,
}

impl Lifecycle {
    pub(crate) fn open(store: &Path, mut status: FolderStatus) -> io::Result<Self> {
        let path = store.with_file_name("lifecycle.json");
        let checkpoint: Checkpoint = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Checkpoint::default(),
            Err(error) => return Err(error),
        };
        if let Some(halt) = &checkpoint.halt {
            status.state = FolderState::Halted;
            status.problem = Some(halt.problem.clone());
        }
        Ok(Self {
            path,
            status: Mutex::new(status),
            checkpoint: Mutex::new(checkpoint),
        })
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
            };
            self.save(&next)?;
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
        let next = Checkpoint::default();
        self.save(&next)?;
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
                problem: "unlinking the folder did not finish; unlink it again to finish".into(),
                recovery: Recovery::Unlink,
            }),
        };
        self.save(&next)?;
        *checkpoint = next;
        Ok(())
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
        checkpoint.halt = Some(Halt {
            problem: problem.clone(),
            recovery,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
                let lifecycle = Lifecycle::open(&store, status.clone()).unwrap();
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
                let reopened = Lifecycle::open(&store, status).unwrap();
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
        let lifecycle = Lifecycle::open(&store, status).unwrap();
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
