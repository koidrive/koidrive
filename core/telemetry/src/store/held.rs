// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

use koidrive_state_file::Flock;
use serde::{Deserialize, Serialize};

use crate::recorder::Pending;
use crate::schema::{HourlyReport, InstallId};

use super::{Batch, HELD, QUEUE, Queue, Queued, Settings, Store, read, remove, write};

/// The reports of a run that asks whether to send them, kept apart from the queue until [`Store::answer`] or
/// [`Store::forget`]. It locks its own file in `held/` while its process lives, so what a run that died kept is known
/// to be unanswered.
#[derive(Debug)]
pub struct Hold {
    id: InstallId,
    pub(super) run: String,
    _lock: Flock,
}

impl Hold {
    /// The install id the reports go under once the answer is yes.
    pub fn id(&self) -> &InstallId {
        &self.id
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Held {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<InstallId>,
    #[serde(default)]
    queue: Queue,
}

impl Store {
    /// Starts keeping this run's reports apart until its question is answered, in a file of its own so that runs
    /// asking at the same time each keep theirs. None once a choice is stored. What runs that died kept is deleted.
    pub fn hold(&self) -> io::Result<Option<Hold>> {
        let _lock = self.lock(true)?;
        let settings = self.settings()?;
        if settings.enabled.is_some() {
            return Ok(None);
        }
        self.remove_unanswered()?;
        let run = InstallId::generate().as_str().to_owned();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(self.dir.join(HELD))?;
        let lock = self.flock(&format!("{HELD}/{run}.lock"), false)?;
        let id = settings.install_id.unwrap_or_else(InstallId::generate);
        let held = Held {
            id: Some(id.clone()),
            queue: Queue::default(),
        };
        write(&self.held_path(&run), &held)?;
        Ok(Some(Hold {
            id,
            run,
            _lock: lock,
        }))
    }

    /// Adds to what `hold` keeps. Dropped once the kept reports were deleted, as by `koi telemetry off`.
    pub fn keep(&self, hold: &Hold, pending: Pending) -> io::Result<()> {
        if pending.hours.values().all(HourlyReport::is_empty) && pending.events.is_empty() {
            return Ok(());
        }
        let _lock = self.lock(true)?;
        let mut held = self.read_held(&hold.run)?;
        if held.id.as_ref() != Some(&hold.id) {
            return Ok(());
        }
        let events = (pending.events.into_iter())
            .map(|event| Queued {
                event,
                origin: self.origin.clone(),
            })
            .collect();
        held.queue.put(pending.hours.into_values(), events);
        held.queue.trim(crate::this_hour());
        write(&self.held_path(&hold.run), &held)
    }

    /// What live runs keep until their questions are answered, each with the install id a yes sends it under.
    pub fn held(&self) -> io::Result<Vec<(InstallId, Batch)>> {
        if !self.dir.join(HELD).exists() {
            return Ok(Vec::new());
        }
        let _lock = self.lock(true)?;
        koidrive_state_file::recover(&self.dir.join(HELD))?;
        self.remove_unanswered()?;
        let mut kept = Vec::new();
        for run in self.held_runs()? {
            let held = self.read_held(&run)?;
            if let Some(id) = held.id {
                let batch = Batch {
                    hours: held.queue.hours.into_values().collect(),
                    events: held.queue.events,
                };
                kept.push((id, batch));
            }
        }
        Ok(kept)
    }

    /// Stores the answer to a run's question. Yes queues what `hold` kept, under the install id stored by then or
    /// else its own. No deletes it with the install id and everything else waiting, as [`Store::set`] does.
    pub fn answer(&self, hold: Hold, on: bool) -> io::Result<Settings> {
        let _lock = self.lock(true)?;
        let held = self.read_held(&hold.run)?;
        self.remove_held(&hold.run)?;
        let settings = self.choose(on, Some(hold.id.clone()))?;
        if on && held.id.is_some() {
            let mut queue = self.queue()?;
            queue.put(held.queue.hours.into_values(), held.queue.events);
            queue.trim(crate::this_hour());
            write(&self.dir.join(QUEUE), &queue)?;
        }
        Ok(settings)
    }

    /// Deletes what `hold` kept, for a question left unanswered.
    pub fn forget(&self, hold: Hold) -> io::Result<()> {
        let _lock = self.lock(true)?;
        self.remove_held(&hold.run)
    }

    pub(super) fn held_path(&self, run: &str) -> PathBuf {
        self.dir.join(HELD).join(format!("{run}.json"))
    }

    fn read_held(&self, run: &str) -> io::Result<Held> {
        match read(&self.held_path(run)) {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(Held::default()),
            read => read,
        }
    }

    fn remove_held(&self, run: &str) -> io::Result<()> {
        remove(&self.held_path(run))?;
        remove(&self.held_path(run).with_extension("json.tmp"))?;
        remove(&self.dir.join(HELD).join(format!("{run}.lock")))
    }

    fn held_runs(&self) -> io::Result<Vec<String>> {
        let entries = match fs::read_dir(self.dir.join(HELD)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            entries => entries?,
        };
        let mut runs = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            for suffix in [".json.tmp", ".json", ".lock"] {
                if let Some(run) = name.strip_suffix(suffix)
                    && !runs.iter().any(|known| known == run)
                {
                    runs.push(run.to_owned());
                }
            }
        }
        Ok(runs)
    }

    fn remove_unanswered(&self) -> io::Result<()> {
        for run in self.held_runs()? {
            match self.flock(&format!("{HELD}/{run}.lock"), false) {
                Ok(_unanswered) => self.remove_held(&run)?,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
