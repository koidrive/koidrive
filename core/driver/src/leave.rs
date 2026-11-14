// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use koidrive_store::Link;
use serde::{Deserialize, Serialize};

use crate::claim::Owner;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Prepared,
    Revoking,
    Committed,
}

#[cfg(test)]
type SyncHook = Box<dyn Fn(&File) -> io::Result<()>>;

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_PHASE: std::cell::Cell<Option<Phase>> = const { std::cell::Cell::new(None) };
    pub(crate) static SYNC_HOOK: std::cell::RefCell<Option<SyncHook>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn sync_dir(dir: &File) -> io::Result<()> {
    #[cfg(test)]
    SYNC_HOOK.with(|hook| match hook.borrow().as_deref() {
        Some(hook) => hook(dir),
        None => Ok(()),
    })?;
    dir.sync_all()
}

pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
    for parent in path.ancestors().skip(1) {
        match File::open(parent) {
            Ok(dir) => return sync_dir(&dir),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl Phase {
    fn byte(self) -> u8 {
        match self {
            Self::Prepared => b'0',
            Self::Revoking => b'2',
            Self::Committed => b'1',
        }
    }

    fn read(byte: u8) -> io::Result<Self> {
        match byte {
            b'0' => Ok(Self::Prepared),
            b'2' => Ok(Self::Revoking),
            b'1' => Ok(Self::Committed),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid sign-out record",
            )),
        }
    }
}

pub(crate) fn phase(state: &Path) -> io::Result<Option<Phase>> {
    let mut file = match File::open(state.join("leaving")) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut byte = [0];
    file.read_exact(&mut byte)?;
    Phase::read(byte[0]).map(Some)
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingLink {
    pub space_id: String,
    pub root: PathBuf,
    pub owner: Owner,
}

impl From<&Link> for PendingLink {
    fn from(link: &Link) -> Self {
        Self {
            space_id: link.space_id.clone(),
            root: link.root.clone(),
            owner: Owner {
                server: link.server_url.clone(),
                device_id: link.device_id.clone(),
            },
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Intent {
    pub links: Vec<PendingLink>,
    pub resume: bool,
    #[serde(default)]
    pub resume_updates: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Saved {
    Intent(Intent),
    Links(Vec<PendingLink>),
}

pub(crate) struct Leaving {
    pub intent: Intent,
    pub phase: Phase,
    file: File,
    pub path: PathBuf,
}

impl Leaving {
    pub fn load(state: &Path) -> io::Result<Option<Self>> {
        let path = state.join("leaving");
        let mut file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let phase =
            Phase::read(*bytes.first().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "empty sign-out record")
            })?)?;
        let intent = match serde_json::from_slice(&bytes[1..]).map_err(io::Error::other)? {
            Saved::Intent(intent) => intent,
            Saved::Links(links) => Intent {
                links,
                ..Default::default()
            },
        };
        Ok(Some(Self {
            intent,
            phase,
            file,
            path,
        }))
    }

    pub fn begin(state: &Path, intent: Intent) -> io::Result<Self> {
        let path = state.join("leaving");
        let next = state.join("leaving.next");
        let mut bytes = vec![b'0'];
        serde_json::to_writer(&mut bytes, &intent).map_err(io::Error::other)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&next)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&next, &path)?;
        File::open(state)?.sync_all()?;
        Ok(Self {
            intent,
            phase: Phase::Prepared,
            file,
            path,
        })
    }

    pub fn set_phase(&mut self, phase: Phase) -> io::Result<()> {
        #[cfg(test)]
        if FAIL_PHASE.with(|failure| failure.get() == Some(phase)) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected sign-out phase failure",
            ));
        }
        self.file.write_all_at(&[phase.byte()], 0)?;
        self.file.sync_all()?;
        self.phase = phase;
        Ok(())
    }

    pub fn finish(self) -> io::Result<()> {
        sync_parent(&self.path)?;
        fs::remove_file(&self.path)?;
        sync_parent(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_parent_sync_keeps_the_journal_until_retry() {
        for phase in [Phase::Prepared, Phase::Committed] {
            let dir = tempfile::tempdir().unwrap();
            let mut leaving = Leaving::begin(dir.path(), Intent::default()).unwrap();
            leaving.set_phase(phase).unwrap();
            SYNC_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(|_| {
                    Err(io::Error::other("injected cleanup directory sync failure"))
                }))
            });
            let result = leaving.finish();
            SYNC_HOOK.with(|hook| *hook.borrow_mut() = None);
            assert!(result.is_err());
            let leaving = Leaving::load(dir.path()).unwrap().unwrap();
            assert!(leaving.phase == phase);
            leaving.finish().unwrap();
            assert!(Leaving::load(dir.path()).unwrap().is_none());
        }
    }

    #[test]
    fn the_whole_intent_survives_a_reopen() {
        for resume in [false, true] {
            for resume_updates in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let intent = Intent {
                    links: vec![PendingLink {
                        space_id: "space".into(),
                        root: dir.path().join("root\nwith a line break"),
                        owner: Owner {
                            server: "https://example.com/".into(),
                            device_id: "device".into(),
                        },
                    }],
                    resume,
                    resume_updates,
                };
                let expected = serde_json::to_value(&intent).unwrap();
                let mut leaving = Leaving::begin(dir.path(), intent).unwrap();
                leaving.set_phase(Phase::Revoking).unwrap();
                drop(leaving);
                for entry in fs::read_dir(dir.path()).unwrap() {
                    let name = entry.unwrap().file_name().into_string().unwrap();
                    assert!(
                        crate::Paths::STATE_ENTRIES.contains(&name.as_str()),
                        "{name}"
                    );
                }
                let leaving = Leaving::load(dir.path()).unwrap().unwrap();
                assert!(leaving.phase == Phase::Revoking);
                assert_eq!(serde_json::to_value(&leaving.intent).unwrap(), expected);
            }
        }
    }

    #[test]
    fn older_intents_default_only_missing_authority() {
        let links = serde_json::json!([{
            "space_id": "space",
            "root": "/root\nwith a line break",
            "owner": { "server": "https://example.com/", "device_id": "device" },
        }]);
        for (body, resume) in [
            (links.clone(), false),
            (
                serde_json::json!({ "links": links, "resume": false }),
                false,
            ),
            (serde_json::json!({ "links": links, "resume": true }), true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("leaving"), format!("0{body}")).unwrap();
            let leaving = Leaving::load(dir.path()).unwrap().unwrap();
            assert!(leaving.phase == Phase::Prepared);
            assert_eq!(leaving.intent.resume, resume);
            assert!(!leaving.intent.resume_updates);
            assert_eq!(serde_json::to_value(&leaving.intent.links).unwrap(), links);
        }
    }

    #[test]
    fn incomplete_intent_objects_are_not_accepted_as_empty_cleanup() {
        for body in ["{}", "{\"links\": []}", "{\"resume\": true}"] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("leaving"), format!("0{body}")).unwrap();
            assert!(Leaving::load(dir.path()).is_err());
        }
    }
}
