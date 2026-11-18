// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use koidrive_state_file::Flock;
use serde::{Deserialize, Serialize};

use crate::consent::{self, Status};
use crate::schema::{InstallId, Origin};

mod held;
mod old;
mod queue;

pub use held::Hold;
pub use old::OldBatch;
use queue::Queue;
pub use queue::{AppCrashStamp, Batch, Queued, Sending};

pub const KEEP_HOURS: u32 = 14 * 24;
pub const MAX_EVENTS: usize = 500;
const QUEUE: &str = "queue-v2.json";
const MAX_APP_CRASH_REPORTS: usize = 64;
const OLD_QUEUE: &str = "queue.json";
const HELD: &str = "held";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The person's choice; none until asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<InstallId>,
    /// Drawn anew each time reports are turned off or reset. An app crash report carries the generation in force when
    /// the app last read the setting and is sent only while that generation is still in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    /// Reports carry `internal: true`, set by `koi telemetry internal on` on the machines of people who build Koidrive.
    #[serde(default, skip_serializing_if = "is_false")]
    pub internal: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Store {
    dir: PathBuf,
    origin: Option<Origin>,
}

impl Store {
    /// The telemetry files of the profile whose state directory is `state_dir`.
    pub fn new(state_dir: &Path) -> Self {
        Store {
            dir: state_dir.join("telemetry"),
            origin: None,
        }
    }

    /// The same files, with events this store queues marked as recorded by `origin`.
    pub fn recorded_by(self, origin: Origin) -> Self {
        Store {
            origin: Some(origin),
            ..self
        }
    }

    /// A missing file is the default: never asked, no id.
    pub fn settings(&self) -> io::Result<Settings> {
        read(&self.dir.join("settings.json"))
    }

    pub fn prompt_pending(&self) -> io::Result<bool> {
        read(&self.dir.join("prompt.json"))
    }

    pub fn begin_prompt(&self) -> io::Result<bool> {
        let _lock = self.lock(true)?;
        if self.settings()?.enabled.is_some() {
            return Ok(false);
        }
        write(&self.dir.join("prompt.json"), &true)?;
        Ok(true)
    }

    /// The mode with this profile's stored choice and the process's environment.
    pub fn status(&self, var: impl Fn(&str) -> Option<std::ffi::OsString>) -> io::Result<Status> {
        Ok(consent::resolve(var, self.settings()?.enabled))
    }

    /// Stores the person's choice. Turning it on keeps the install id or draws one. Turning it off deletes the id and
    /// everything waiting to be sent. A `settings.json` that cannot be parsed is replaced. Whenever no id is kept, the
    /// queue goes too.
    pub fn set(&self, on: bool) -> io::Result<Settings> {
        let _lock = self.lock(true)?;
        self.choose(on, None)
    }

    fn choose(&self, on: bool, id: Option<InstallId>) -> io::Result<Settings> {
        let (mut settings, replaced) = match self.settings() {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => (Settings::default(), true),
            settings => (settings?, false),
        };
        if !on || replaced {
            settings.generation = Some(new_generation());
        }
        settings.enabled = Some(on);
        settings.install_id = settings.install_id.filter(|_| on);
        if settings.install_id.is_none() {
            self.remove_queues()?;
        }
        if !on {
            self.remove_every_held()?;
        }
        if on {
            settings
                .install_id
                .get_or_insert_with(|| id.unwrap_or_else(InstallId::generate));
        }
        write(&self.dir.join("settings.json"), &settings)?;
        remove(&self.dir.join("prompt.json"))?;
        Ok(settings)
    }

    /// Marks reports internal or not, whether or not they are on.
    pub fn mark_internal(&self, internal: bool) -> io::Result<Settings> {
        let _lock = self.lock(true)?;
        let mut settings = self.settings()?;
        settings.internal = internal;
        write(&self.dir.join("settings.json"), &settings)?;
        Ok(settings)
    }

    /// Whether reports are marked internal, with this profile's mark and the process's environment.
    pub fn internal(&self, var: impl Fn(&str) -> Option<std::ffi::OsString>) -> io::Result<bool> {
        Ok(consent::internal(var, self.settings()?.internal))
    }

    /// Deletes everything waiting to be sent and then draws a new install id, or none without an id.
    pub fn reset(&self) -> io::Result<Option<InstallId>> {
        let _lock = self.lock(true)?;
        self.remove_queues()?;
        self.remove_every_held()?;
        let mut settings = self.settings()?;
        let id = settings.install_id.as_ref().map(|_| InstallId::generate());
        settings.install_id = id.clone();
        settings.generation = Some(new_generation());
        write(&self.dir.join("settings.json"), &settings)?;
        Ok(id)
    }

    /// The install id, drawn and stored when telemetry is on through `KOIDRIVE_TELEMETRY` and none exists yet.
    pub fn install_id(&self) -> io::Result<InstallId> {
        let _lock = self.lock(true)?;
        let mut settings = self.settings()?;
        if let Some(id) = &settings.install_id {
            return Ok(id.clone());
        }
        self.remove_queues()?;
        let id = InstallId::generate();
        settings.install_id = Some(id.clone());
        write(&self.dir.join("settings.json"), &settings)?;
        Ok(id)
    }

    fn remove_every_held(&self) -> io::Result<()> {
        match fs::remove_dir_all(self.dir.join(HELD)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    fn remove_queues(&self) -> io::Result<()> {
        remove(&self.dir.join(QUEUE))?;
        remove(&self.dir.join(OLD_QUEUE))
    }

    fn queue(&self) -> io::Result<Queue> {
        match read(&self.dir.join(QUEUE)) {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                tracing::debug!(%error, "dropping a telemetry queue that cannot be parsed");
                Ok(Queue::default())
            }
            read => read,
        }
    }

    fn lock(&self, wait: bool) -> io::Result<Flock> {
        let lock = self.flock("lock", wait)?;
        koidrive_state_file::recover(&self.dir)?;
        for name in [
            "settings.json.tmp",
            "prompt.json.tmp",
            "queue.json.tmp",
            "queue-v2.json.tmp",
        ] {
            remove(&self.dir.join(name))?;
        }
        Ok(lock)
    }

    fn send_lock(&self) -> io::Result<Flock> {
        self.flock("send.lock", false)
    }

    fn flock(&self, name: &str, wait: bool) -> io::Result<Flock> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(name))?;
        if wait {
            return Flock::wait(file);
        }
        Flock::take(file)?.ok_or_else(|| io::ErrorKind::WouldBlock.into())
    }
}

fn new_generation() -> String {
    InstallId::generate().as_str().to_owned()
}

fn read<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error),
    }
}

fn write<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    koidrive_state_file::write(path, &serde_json::to_vec(value)?)
}

fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
