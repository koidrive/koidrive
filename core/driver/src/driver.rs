// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use koidrive_engine::commit::{Commits, Lift, LiftError};
use koidrive_engine::guard::Window;
use koidrive_engine::ignore::LocalRules;
use koidrive_engine::tree::Kind;
use koidrive_fs::{Marker, probe_no_replace, read_marker, root_at, write_marker};
use koidrive_ignore::LocalRulesError;
use koidrive_net::{ApiError, Auth, ROTATION_PERIOD, ServerError, UploadGate};
use koidrive_proto::v1::{ClaimRequest, DeviceCredentials, PairCompleteRequest, SpaceInfo};
use koidrive_state_file::{Flock, Published};
use koidrive_store::{AskedRestore, EngineState, GivenAnswer, Link, Store, StoredQuestion};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, watch};

use crate::claim::{self, Owner};
use crate::config::{Config, ConfigError};
use crate::credential::{
    Credential, CredentialError, CredentialFiles, NewCredentialFile, StoredCredential, api_for,
    next_path,
};
use crate::disk::{self, RelinkError, Stop};
use crate::failures::Halted;
use crate::folder::FolderTask;
use crate::ignore::{GitignoreOnly, Matcher, device, has_sync_entries, local_rules, matcher};
use crate::leave::{Leaving, Phase};
use crate::paths::{Paths, PathsError};
use crate::pending_create::{self, Intent, PendingCreate};
use crate::question::{Answer, Limit, Question, QuestionKind};
use crate::restore::{self, RestoreRequest, RestoreStatus, Seen};
use crate::status::{Account, Activity, FolderState, FolderStatus, Status};
use crate::update::{self, Update, Updates};

mod account;
mod error;
mod folders;
mod link;
mod questions;

use account::rotating_auth;
pub use account::{SetAside, SignedOut, Unconfirmed, lossy_paths};
pub use error::{CleanupFailure, DriverError};
pub(crate) use error::{io_error, lift_halt, store_error};
use folders::{Loaded, finish_removals, load_folders, read_folders};
pub(crate) use link::check_placement;
pub use link::{LinkApproval, Linked};
use link::{Unlink, forget_unreported_set_aside};
pub use questions::Answered;

/// Runs sync for the folders one profile links.
///
/// Opening a driver takes the profile's lock, so one process at a time writes a profile's credential and stores.
/// The lock is released when the last clone is dropped.
#[derive(Clone)]
pub struct Driver {
    inner: Arc<Inner>,
}

struct Inner {
    telemetry: koidrive_telemetry::Recorder,
    paths: Paths,
    _lock: Flock,
    rules: Mutex<DeviceRules>,
    /// The size cap in bytes of each folder's local trash, from `[trash]`.
    trash_cap: u64,
    upload_gate: UploadGate,
    /// Held for the whole of a pair, claim, sign-out, link or unlink, so one runs at a time: a sign-out waits for a
    /// claim or a link in flight and then undoes it, and a credential the server issued is always stored.
    lifecycle: tokio::sync::Mutex<()>,
    pausing: Arc<tokio::sync::Mutex<()>>,
    credential: Mutex<Option<Credential>>,
    /// The one [`Auth`] of this process while it is signed in, which every caller uses, so that a rotation of the
    /// refresh credential reaches all of them (RFC 0011 section 2). None while signed out, or when the credential's
    /// server URL cannot be used.
    auth: Mutex<Option<Arc<Auth>>>,
    /// Held by every write to the credential files and by the sign-out while it removes them (`CredentialFiles`).
    credential_writes: Arc<Mutex<()>>,
    folders: Mutex<BTreeMap<String, Folder>>,
    unplaced: Mutex<BTreeMap<String, Arc<crate::lifecycle::Lifecycle>>>,
    running: AtomicBool,
    paused: AtomicBool,
    updates: Arc<Updates>,
    startup_blocked: AtomicBool,
    startup_pending: AtomicBool,
    changed: Arc<watch::Sender<()>>,
    progressed: Arc<watch::Sender<()>>,
    totals: Arc<Mutex<crate::progress::Sum>>,
    devices: Mutex<Devices>,
    #[cfg(test)]
    observe_fault: Option<Arc<crate::folder::ObserveFault>>,
    #[cfg(test)]
    restart_stopped: Mutex<Option<RestartStopped>>,
}

struct DeviceRules {
    matcher: Arc<dyn Matcher>,
    local: LocalRules,
}

#[derive(Default)]
struct Devices {
    names: HashMap<String, String>,
    listed_at: Option<Instant>,
    listing: Option<tokio::task::AbortHandle>,
}

#[cfg(test)]
struct RestartStopped {
    reached: tokio::sync::oneshot::Sender<()>,
    proceed: tokio::sync::oneshot::Receiver<()>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.updates.stop();
        if let Some(listing) = self.devices.get_mut().unwrap().listing.take() {
            listing.abort();
        }
    }
}

/// A linked folder. It stays in the driver's map until unlinking it has finished, so a stop, an unlink or a sign-out
/// that starts later still finds its task and its cleanup, also after an earlier caller gave up waiting.
struct Folder {
    linked_started: Option<crate::timing::Time>,
    link: Link,
    status: Arc<crate::lifecycle::Lifecycle>,
    /// The folder's task, from its start until a stop has seen it end.
    run: Option<Running>,
    /// Set from the moment an unlink begins until it has removed the folder or failed. No task starts for the folder
    /// meanwhile, also after its last task has ended, so the cleanup never leaves a task it does not wait for.
    unlink: Option<Unlink>,
    store: Arc<RwLock<()>>,
    progress: Arc<Mutex<crate::progress::Tracker>>,
    observing: Arc<Mutex<Option<crate::observe::Observing>>>,
    brought_back: Vec<PathBuf>,
}

struct Running {
    stop: watch::Sender<bool>,
    requests: Arc<Notify>,
    /// Closed when the task has ended, its observer and its hold on the folder included.
    done: watch::Receiver<()>,
}

impl Running {
    fn stop(&self) -> watch::Receiver<()> {
        let _ = self.stop.send(true);
        self.done.clone()
    }

    fn ended(&self) -> bool {
        self.done.has_changed().is_err()
    }
}

/// How often [`Changes::next`] reports a change of progress alone at most.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// What [`Changes::next`] waited for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// [`Driver::watch`] was marked: anything [`Driver::status`], [`Driver::questions`] or [`Driver::restores`]
    /// return may differ, progress included.
    State,
    /// Only the progress of a folder in [`Driver::status`] may differ.
    Progress,
}

/// The changes of a driver, from [`Driver::changes`].
pub struct Changes {
    changed: watch::Receiver<()>,
    progressed: watch::Receiver<()>,
    last: Option<tokio::time::Instant>,
    pending: bool,
}

impl Changes {
    /// Waits for the next change. A [`Change::State`] returns at once. A [`Change::Progress`] returns no sooner than
    /// [`PROGRESS_INTERVAL`] after the previous change this returned, and a state change meanwhile returns instead.
    /// Fails once the driver is gone.
    pub async fn next(&mut self) -> Result<Change, watch::error::RecvError> {
        if !self.pending {
            let state = tokio::select! {
                biased;
                changed = self.changed.changed() => changed.map(|()| true)?,
                progressed = self.progressed.changed() => progressed.map(|()| false)?,
            };
            if state {
                return Ok(self.returned(Change::State));
            }
            self.pending = true;
        }
        if let Some(last) = self.last {
            let due = last + PROGRESS_INTERVAL;
            let state = tokio::select! {
                biased;
                changed = self.changed.changed() => changed.map(|()| true)?,
                () = tokio::time::sleep_until(due) => false,
            };
            if state {
                return Ok(self.returned(Change::State));
            }
        }
        Ok(self.returned(Change::Progress))
    }

    fn returned(&mut self, change: Change) -> Change {
        self.pending = false;
        self.progressed.mark_unchanged();
        self.last = Some(tokio::time::Instant::now());
        change
    }
}

async fn ended(mut done: watch::Receiver<()>) {
    while done.changed().await.is_ok() {}
}

impl Driver {
    /// The folders the profile at `paths` links, read without writing anything: no directory, lock file or store is
    /// created or changed. The stores are read under a shared lock on the profile's lock file when it exists, which keeps
    /// writers out, so a profile that another process holds is refused with [`DriverError::Busy`].
    pub fn linked_folders(paths: &Paths) -> Result<Vec<FolderStatus>, DriverError> {
        let lock_path = paths.lock();
        let _lock = match File::open(&lock_path) {
            Ok(file) => match Flock::share(file).map_err(io_error(&lock_path))? {
                Some(lock) => Some(lock),
                None => return Err(DriverError::Busy(lock_path)),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_error(&lock_path)(error)),
        };
        Ok(
            read_folders(paths, &koidrive_telemetry::Recorder::off(), |path| {
                Store::open_read_only(path)
            })?
            .into_iter()
            .map(|found| found.status.lock().unwrap().clone())
            .collect(),
        )
    }

    /// Opens the profile at `paths`: takes its lock, reads its configuration and credential, and finds its linked
    /// folders. Nothing syncs until [`Driver::start`].
    pub fn open(paths: Paths) -> Result<Self, DriverError> {
        Self::open_with_telemetry(paths, koidrive_telemetry::Recorder::off())
    }

    pub fn open_with_telemetry(
        paths: Paths,
        telemetry: koidrive_telemetry::Recorder,
    ) -> Result<Self, DriverError> {
        paths
            .create_state_dir(&paths.state_dir)
            .map_err(io_error(&paths.state_dir))?;
        let lock_path = paths.lock();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(io_error(&lock_path))?;
        let Some(lock) = Flock::take(lock).map_err(io_error(&lock_path))? else {
            return Err(DriverError::Busy(lock_path));
        };
        finish_removals(&paths);
        let config = Config::load(&paths.config_file)?;
        let invalid = |error: LocalRulesError| ConfigError::Invalid {
            path: paths.config_file.clone(),
            reason: error.to_string(),
        };
        let local = local_rules(&config.sync).map_err(invalid)?;
        let matcher = matcher(&config.sync).map_err(invalid)?;
        let leaving =
            Leaving::load(&paths.state_dir).map_err(io_error(&paths.state_dir.join("leaving")))?;
        let blocked = leaving
            .as_ref()
            .is_some_and(|leaving| leaving.phase != Phase::Prepared);
        let stored = if blocked {
            None
        } else {
            Credential::load_stored(&paths.credential())?
        };
        if let Some(stored) = &stored {
            Credential::keep_out_of_backups(&paths.credential())?;
            if stored.staged.is_some() {
                Credential::keep_out_of_backups(&next_path(&paths.credential()))?;
            }
        }
        let credential_writes = Arc::default();
        let auth = stored
            .as_ref()
            .and_then(|stored| rotating_auth(&paths, stored, &credential_writes).ok());
        let credential = stored.map(|stored| stored.credential);
        if credential.is_some() {
            forget_unreported_set_aside(&paths);
        }
        let loaded = if blocked {
            Loaded::default()
        } else {
            load_folders(&paths, &telemetry)?
        };
        let paused = paths.paused().is_file();
        let variable = std::env::var(update::VARIABLE).ok();
        let updates = Updates::open(
            update::switched_on(&config.updates, variable.as_deref()),
            paths.update_check(),
        );
        Ok(Driver {
            inner: Arc::new(Inner {
                telemetry,
                rules: Mutex::new(DeviceRules { matcher, local }),
                trash_cap: config.trash.cap(),
                upload_gate: UploadGate::new(config.upload.concurrency.get()),
                paths,
                _lock: lock,
                lifecycle: tokio::sync::Mutex::new(()),
                pausing: Arc::default(),
                credential: Mutex::new(credential),
                auth: Mutex::new(auth),
                credential_writes,
                folders: Mutex::new(loaded.folders),
                unplaced: Mutex::new(loaded.unplaced),
                running: AtomicBool::new(false),
                paused: AtomicBool::new(paused),
                updates: Arc::new(updates),
                startup_blocked: AtomicBool::new(false),
                startup_pending: AtomicBool::new(false),
                changed: Arc::new(watch::Sender::new(())),
                progressed: Arc::new(watch::Sender::new(())),
                totals: Arc::default(),
                devices: Mutex::default(),
                #[cfg(test)]
                observe_fault: None,
                #[cfg(test)]
                restart_stopped: Mutex::default(),
            }),
        })
    }

    pub fn paths(&self) -> &Paths {
        &self.inner.paths
    }

    pub fn status(&self) -> Status {
        let account = self
            .inner
            .credential
            .lock()
            .unwrap()
            .as_ref()
            .map(Credential::account);
        let mut folders: Vec<FolderStatus> = self
            .inner
            .folders
            .lock()
            .unwrap()
            .values()
            .map(|folder| {
                let mut status = folder.status.lock().unwrap().clone();
                if folder.unlink == Some(Unlink::Abandoned) {
                    status.state = FolderState::Halted;
                    status.problem = Some(crate::lifecycle::UNFINISHED_UNLINK.into());
                }
                if !matches!(
                    status.state,
                    FolderState::Bootstrapping | FolderState::CatchingUp
                ) {
                    status.progress = None;
                    status.activity = None;
                }
                status.activity =
                    Activity::live(status.activity, *folder.observing.lock().unwrap());
                status.bytes_moved = folder.progress.lock().unwrap().moved();
                status
            })
            .collect();
        folders.extend(
            (self.inner.unplaced.lock().unwrap().values())
                .map(|status| status.lock().unwrap().clone()),
        );
        Status {
            account,
            running: self.inner.running.load(Ordering::SeqCst),
            paused: self.inner.paused.load(Ordering::SeqCst),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            update: self.inner.updates.notice(),
            progress: (folders.iter().any(|folder| folder.progress.is_some()))
                .then(|| self.inner.totals.lock().unwrap().totals())
                .flatten(),
            folders,
        }
    }

    /// Keeps each linked folder's sync health in this hour's telemetry report: the folders halted, why and for how long,
    /// how long ago each was last up to date, and the nodes left to sync.
    pub fn record_health(&self) {
        self.record_health_at(koidrive_telemetry::now());
    }

    fn record_health_at(&self, now: u64) {
        let mut lifecycles: Vec<_> = (self.inner.folders.lock().unwrap().values())
            .map(|folder| folder.status.clone())
            .collect();
        lifecycles.extend(self.inner.unplaced.lock().unwrap().values().cloned());
        let folders: Vec<_> = (lifecycles.iter())
            .map(|lifecycle| lifecycle.health(now))
            .collect();
        crate::health::record(
            &self.inner.telemetry,
            now,
            self.inner.paused.load(Ordering::SeqCst),
            &folders,
        );
    }

    /// A receiver that is marked changed whenever [`Driver::status`], [`Driver::questions`] or [`Driver::restores`]
    /// may have changed, so the app can wait for a change instead of polling. A change may be signalled without a
    /// visible difference. A change of a folder's progress alone does not mark it, unless the progress began or ended.
    pub fn watch(&self) -> watch::Receiver<()> {
        self.inner.changed.subscribe()
    }

    /// The changes [`Driver::watch`] marks and those of every folder's progress, at a bounded rate.
    pub fn changes(&self) -> Changes {
        Changes {
            changed: self.inner.changed.subscribe(),
            progressed: self.inner.progressed.subscribe(),
            last: None,
            pending: false,
        }
    }

    fn changed(&self) {
        self.inner.changed.send_replace(());
    }

    /// Starts asking GitHub, on the current Tokio runtime, whether a newer Koidrive release is out: when it is a day
    /// since GitHub last answered, and again after that. Only the process that syncs the profile calls it, and it keeps
    /// the answer in the profile for every [`Driver::status`]. With the check switched off it removes that answer
    /// instead. Calling it while the check runs does nothing, and [`Driver::stop`] stops it.
    pub fn check_for_updates(&self) {
        self.inner.updates.start(self.inner.changed.clone());
    }

    /// Asks GitHub now whether a newer Koidrive release is out. The answer is kept only while update checks are on.
    pub async fn check_for_updates_now(&self) -> Result<Update, update::CheckError> {
        self.inner.updates.check_now(&self.inner.changed).await
    }

    fn space_ids(&self) -> Vec<String> {
        self.inner.folders.lock().unwrap().keys().cloned().collect()
    }

    fn halted(&self, space_id: &str) -> bool {
        (self.inner.folders.lock().unwrap().get(space_id))
            .is_some_and(|folder| folder.status.halted())
    }

    fn read_store<T>(
        &self,
        space_id: &str,
        read: impl FnOnce(&mut Store) -> Result<T, koidrive_store::Error>,
    ) -> Result<Option<T>, DriverError> {
        let path = self.inner.paths.store(space_id)?;
        let folders = self.inner.folders.lock().unwrap();
        let Some(store) = (folders.get(space_id))
            .filter(|folder| folder.unlink != Some(Unlink::Running))
            .map(|folder| folder.store.clone())
        else {
            return Ok(None);
        };
        let _reading = store.read().unwrap_or_else(PoisonError::into_inner);
        drop(folders);
        (Store::open_existing(&path).and_then(|mut store| read(&mut store)))
            .map(Some)
            .map_err(store_error(&path))
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|dir| dir.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
