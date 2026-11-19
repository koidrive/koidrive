// SPDX-License-Identifier: Apache-2.0

use super::link::{find, owner_of, recorded_brought_back, roots};
use super::*;

fn write_gitignore(config_file: &Path, follow: bool) -> Result<(), DriverError> {
    let path = &linked_file(config_file).map_err(io_error(config_file))?;
    let (text, mode) = match koidrive_state_file::read_to_string(path) {
        Ok(text) => {
            let mode = fs::metadata(path).map_err(io_error(path))?.permissions();
            (text, Some(mode))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (String::new(), None),
        Err(error) => return Err(io_error(path)(error)),
    };
    let invalid = |reason: String| ConfigError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error: toml_edit::TomlError| invalid(error.to_string()))?;
    let sync = (document.entry("sync").or_insert_with(toml_edit::table))
        .as_table_like_mut()
        .ok_or_else(|| invalid("sync is not a table".into()))?;
    let already = sync.get("gitignore").and_then(toml_edit::Item::as_bool) == Some(follow);
    sync.insert("gitignore", toml_edit::value(follow));
    let text = document.to_string();
    Config::parse(path, &text)?;
    if already {
        return Ok(());
    }
    if let Some(dir) = config_file.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io_error(dir))?;
    }
    let stage = koidrive_state_file::AtomicFile::prepare(path).map_err(io_error(path))?;
    stage
        .as_file()
        .write_all(text.as_bytes())
        .map_err(io_error(path))?;
    if let Some(mode) = mode {
        stage
            .as_file()
            .set_permissions(mode)
            .map_err(io_error(path))?;
    }
    stage
        .finish(koidrive_state_file::Publish::Replace)
        .map_err(io_error(path))?
        .warn_unless_durable(path);
    Ok(())
}

fn linked_file(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {}
        _ => return Ok(path.to_path_buf()),
    }
    match fs::canonicalize(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        resolved => return resolved,
    }
    let mut target = path.to_path_buf();
    let mut hops = 0;
    while fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        hops += 1;
        if hops > 40 {
            return Err(rustix::io::Errno::LOOP.into());
        }
        target = target
            .parent()
            .unwrap_or(Path::new(""))
            .join(fs::read_link(&target)?);
    }
    let last = target
        .as_os_str()
        .as_bytes()
        .rsplit(|byte| *byte == b'/')
        .next();
    let name = match target.components().next_back() {
        Some(Component::Normal(name)) if last == Some(name.as_bytes()) => name,
        _ => return Err(io::Error::from(io::ErrorKind::NotFound)),
    };
    let parent = target.parent().unwrap_or(Path::new(""));
    Ok(fs::canonicalize(parent)?.join(name))
}

enum Fix {
    Lift(Lift),
    Relink,
}

fn relink(link: &Link, store: &Path) -> Result<u64, DriverError> {
    let root = &link.root;
    let lock = claim::lock(root)
        .map_err(io_error(root))?
        .ok_or_else(|| DriverError::FolderInUse(root.clone()))?;
    match claim::owner(root).map_err(io_error(root))? {
        Some(other) if other != owner_of(link) => {
            return Err(DriverError::FolderOfOtherDevice {
                device_id: other.device_id,
                server: other.server,
            });
        }
        _ => {}
    }
    let mut opened = Store::open_existing(store).map_err(store_error(store))?;
    disk::relink(&lock, link, &mut opened).map_err(|error| match error {
        RelinkError::NotACopy => DriverError::NotACopy(root.clone()),
        RelinkError::Stop(Stop::Io(error)) => io_error(root)(error),
        RelinkError::Stop(Stop::Store(error)) => store_error(store)(error),
        RelinkError::Stop(stop) => DriverError::NotRelinked {
            path: root.clone(),
            why: stop.to_string(),
        },
    })
}

pub(super) struct BlockedStartup<'a>(pub(super) &'a Driver);

impl Drop for BlockedStartup<'_> {
    fn drop(&mut self) {
        let pending = {
            let _folders = self.0.inner.folders.lock().unwrap();
            self.0.inner.startup_blocked.store(false, Ordering::SeqCst);
            self.0.inner.startup_pending.swap(false, Ordering::SeqCst)
        };
        let signed_in = self.0.inner.credential.lock().unwrap().is_some();
        if pending && signed_in {
            self.0.start_folders();
        }
        self.0.changed();
    }
}

pub(super) fn halt(folder: &mut Folder, why: Halted, problem: String) {
    tracing::warn!(
        space_id = folder.link.space_id,
        problem,
        "a folder cannot sync"
    );
    let recovery = match why {
        Halted::Unlink => crate::status::Remedy::Unlink,
        _ => crate::status::Remedy::Restart,
    };
    folder.status.halt(why, problem, recovery);
}

pub(super) fn folder_status(
    link: &Link,
    cursor: Option<koidrive_store::Cursor>,
    nodes: u64,
    questions: u64,
) -> FolderStatus {
    FolderStatus {
        space_name: link.space_name.clone(),
        path: Some(link.root.clone()),
        history_id: cursor.as_ref().map(|cursor| cursor.history_id.clone()),
        last_seq: cursor.map(|cursor| cursor.last_seq),
        remote_nodes: nodes,
        questions,
        ..stopped_status(&link.space_id)
    }
}

fn stopped_status(space_id: &str) -> FolderStatus {
    FolderStatus {
        recovery: None,
        space_id: space_id.to_owned(),
        space_name: String::new(),
        path: None,
        state: FolderState::Stopped,
        history_id: None,
        last_seq: None,
        head_seq: None,
        remote_nodes: 0,
        local_entries: None,
        questions: 0,
        problem: None,
        relinkable: false,
        remedy: None,
        wait: None,
        progress: None,
        held_disks: Vec::new(),
        activity: None,
        bytes_moved: 0,
        kept_entries: Vec::new(),
    }
}

#[derive(Default)]
pub(super) struct Loaded {
    pub(super) folders: BTreeMap<String, Folder>,
    pub(super) unplaced: BTreeMap<String, Arc<crate::lifecycle::Lifecycle>>,
}

pub(super) fn load_folders(
    paths: &Paths,
    telemetry: &koidrive_telemetry::Recorder,
) -> Result<Loaded, DriverError> {
    let mut loaded = Loaded::default();
    for found in read_folders(paths, telemetry, |path| Store::open_existing(path))? {
        found.status.settle_unreadable();
        let link = match found.place {
            Place::Store(link) => {
                if crate::link_copy::read(&found.store, &found.space_id).as_ref() != Some(&link)
                    && let Err(error) = crate::link_copy::write(&found.store, &link)
                {
                    tracing::warn!(path = %koidrive_fs::quoted(&crate::link_copy::path(&found.store)), %error, "the copy of a folder's link could not be saved");
                }
                link
            }
            Place::Copy(link) => link,
            Place::Unknown => {
                loaded.unplaced.insert(found.space_id, found.status);
                continue;
            }
        };
        loaded.folders.insert(
            link.space_id.clone(),
            Folder {
                linked_started: None,
                status: found.status,
                link,
                run: None,
                unlink: None,
                store: Arc::default(),
                progress: Arc::default(),
                observing: Arc::default(),
                brought_back: recorded_brought_back(&found.store),
            },
        );
    }
    Ok(loaded)
}

pub(super) fn remove_store_dir(paths: &Paths, space_id: &str) -> io::Result<Published> {
    let dir = paths.spaces().join(space_id);
    let mut attempt = 1;
    let removing = loop {
        let removing = paths.removing(space_id, attempt);
        match rustix::fs::renameat(rustix::fs::CWD, &dir, rustix::fs::CWD, &removing) {
            Ok(()) => break removing,
            Err(rustix::io::Errno::NOENT) => {
                crate::leave::sync_parent(&dir)?;
                return Ok(Published {
                    value: (),
                    durable: Ok(()),
                });
            }
            Err(rustix::io::Errno::EXIST | rustix::io::Errno::NOTEMPTY) => attempt += 1,
            Err(error) => return Err(error.into()),
        }
    };
    let durable = crate::leave::sync_parent(&dir);
    if durable.is_ok() {
        finish_removing(&removing);
    }
    Ok(Published { value: (), durable })
}

pub(super) fn finish_removals(paths: &Paths) {
    let Ok(entries) = fs::read_dir(paths.spaces()) else {
        return;
    };
    for entry in entries.flatten() {
        if Paths::is_removing(&entry.file_name()) {
            finish_removing(&entry.path());
        }
    }
}

fn finish_removing(dir: &Path) {
    if let Err(error) = fs::remove_dir_all(dir) {
        tracing::warn!(path = %koidrive_fs::quoted(dir), %error, "the sync state of a removed folder could not be deleted; the next start tries again");
    }
}

enum Place {
    Store(Link),
    Copy(Link),
    Unknown,
}

pub(super) struct Found {
    space_id: String,
    store: PathBuf,
    place: Place,
    pub(super) status: Arc<crate::lifecycle::Lifecycle>,
}

pub(super) fn read_folders(
    paths: &Paths,
    telemetry: &koidrive_telemetry::Recorder,
    open: impl Fn(&Path) -> Result<Store, koidrive_store::Error>,
) -> Result<Vec<Found>, DriverError> {
    let spaces = paths.spaces();
    let entries = match fs::read_dir(&spaces) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error(&spaces)(error)),
    };
    let mut folders = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error(&spaces))?;
        let space_id = entry.file_name().to_string_lossy().into_owned();
        let Ok(store) = paths.store(&space_id) else {
            continue;
        };
        if let Ok(false) = store.try_exists() {
            continue;
        }
        let read = open(&store).and_then(|mut opened| {
            opened.transaction(|tx| {
                let questions = tx
                    .questions()?
                    .iter()
                    .filter(|q| q.answer.is_none())
                    .count();
                Ok::<_, koidrive_store::Error>((
                    tx.link()?,
                    tx.cursor()?,
                    tx.remote_node_count()?,
                    questions,
                ))
            })
        });
        let lifecycle = |status| {
            Arc::new(crate::lifecycle::Lifecycle::open(
                &store,
                status,
                telemetry.clone(),
            ))
        };
        let (place, status) = match read {
            Ok((Some(link), cursor, nodes, questions)) => {
                let status = lifecycle(folder_status(&link, cursor, nodes, questions as u64));
                (Place::Store(link), status)
            }
            Ok((None, ..)) => {
                let Some(link) = crate::link_copy::read(&store, &space_id) else {
                    tracing::warn!(path = %koidrive_fs::quoted(&store), "a store records no linked folder; ignoring it");
                    continue;
                };
                let status = lifecycle(folder_status(&link, None, 0, 0));
                status.store_unopened(crate::lifecycle::unopened_because(
                    &store,
                    "it records no linked folder",
                ));
                (Place::Copy(link), status)
            }
            Err(
                error @ (koidrive_store::Error::SchemaTooOld { .. }
                | koidrive_store::Error::LogNotApplied(_)),
            ) => return Err(store_error(&store)(error)),
            Err(error) => {
                tracing::warn!(path = %koidrive_fs::quoted(&store), %error, "a folder's sync database cannot be opened");
                match crate::link_copy::read(&store, &space_id) {
                    Some(link) => {
                        let status = lifecycle(folder_status(&link, None, 0, 0));
                        status.store_unopened(crate::lifecycle::unopened(&store, &error));
                        (Place::Copy(link), status)
                    }
                    None => {
                        let status = lifecycle(stopped_status(&space_id));
                        status.store_unopened(crate::lifecycle::unopened_unplaced(
                            &store, &error, &space_id,
                        ));
                        (Place::Unknown, status)
                    }
                }
            }
        };
        folders.push(Found {
            space_id,
            store,
            place,
            status,
        });
    }
    Ok(folders)
}

impl Driver {
    /// Starts syncing every linked folder, each in a task on the current Tokio runtime, unless sync is paused: then
    /// the folders start at [`Driver::resume`]. Starting a running driver does nothing.
    pub fn start(&self) {
        if self.inner.running.swap(true, Ordering::SeqCst) {
            return;
        }
        self.start_folders();
        self.changed();
    }

    /// Pauses sync: stops every folder as [`Driver::stop`] does and keeps the profile's paused file, so that a driver
    /// opened on the profile later stays paused too. Returns false when sync was paused already. Once it holds the pause
    /// lock, the pause runs in a task of its own on the current Tokio runtime, so it finishes stopping the folders even
    /// when its caller stops waiting, and a later resume waits until it has. A pause dropped before it holds the lock
    /// changes nothing.
    pub async fn pause(&self) -> Result<bool, DriverError> {
        let pausing = self.inner.pausing.clone().lock_owned().await;
        let driver = self.clone();
        let paused = tokio::spawn(async move {
            let _pausing = pausing;
            let path = driver.inner.paths.paused();
            let written = match koidrive_state_file::write(&path, b"paused\n") {
                Ok(published) => published.durable,
                Err(error) if path.is_file() => Err(error),
                Err(error) => return Err(io_error(&path)(error)),
            };
            let changed = !driver.inner.paused.swap(true, Ordering::SeqCst);
            if changed {
                driver.stop_folders().await;
            }
            let persisted =
                (driver.inner.folders.lock().unwrap().values()).try_for_each(|folder| {
                    folder
                        .status
                        .persist()
                        .map_err(io_error(folder.status.path()))
                });
            if let Err(error) = written {
                if let Err(folder) = &persisted {
                    tracing::warn!(%folder, "a folder's paused state could not be saved either");
                }
                return Err(DriverError::PausedUnsaved { path, error });
            }
            persisted.map(|()| changed)
        });
        paused
            .await
            .unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()))
    }

    /// Ends a pause: removes the profile's paused file and starts the folders if the driver was started. Returns false
    /// when sync was not paused.
    pub async fn resume(&self) -> Result<bool, DriverError> {
        let _pausing = self.inner.pausing.lock().await;
        let path = self.inner.paths.paused();
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(_) if !path.is_file() => {}
            Err(error) => return Err(io_error(&path)(error)),
        }
        if !self.inner.paused.swap(false, Ordering::SeqCst) {
            return Ok(false);
        }
        self.start_folders();
        self.changed();
        Ok(true)
    }

    pub(super) fn syncs(&self) -> bool {
        self.inner.running.load(Ordering::SeqCst)
            && !self.inner.paused.load(Ordering::SeqCst)
            && !self.inner.startup_blocked.load(Ordering::SeqCst)
    }

    fn start_folders(&self) {
        let mut folders = self.inner.folders.lock().unwrap();
        if self.inner.startup_blocked.load(Ordering::SeqCst) {
            self.inner.startup_pending.store(true, Ordering::SeqCst);
            return;
        }
        if !self.syncs() {
            return;
        }
        let credential = self.inner.credential.lock().unwrap().clone();
        let all = roots(&folders);
        for folder in folders.values_mut() {
            self.start_folder(folder, credential.as_ref(), &all);
        }
    }

    /// Syncs the folder at `folder` again from the start: stops its task, waits for it to end, and starts a new one.
    /// A folder halted by a problem only a person can fix stays halted until this is called. While the driver is
    /// stopped, the folder only stops, and the next start syncs it.
    pub async fn restart(&self, folder: &Path) -> Result<FolderStatus, DriverError> {
        self.restart_with(folder, None).await
    }

    /// Lifts the halt a refusal of the server put on the folder at `folder` the way a person chose, and syncs it
    /// again as [`Driver::restart`] does. [`Lift::Resend`] sends the refused commit again, unchanged;
    /// [`Lift::Replan`] plans its changes again into a new commit. A folder with no such halt, or whose halt waits
    /// for the space's owner to confirm the changes, keeps its state, is restarted all the same, and the error
    /// says why nothing was lifted.
    pub async fn lift(&self, folder: &Path, lift: Lift) -> Result<FolderStatus, DriverError> {
        self.restart_with(folder, Some(Fix::Lift(lift))).await
    }

    /// Takes the folder at `folder`, halted as an older copy of the folder it links or after its store was restored on
    /// its own, as it is now, and syncs it again as [`Driver::restart`] does. The next sync matches
    /// it against the server's tree as a first link does: nothing it lacks is deleted on the server. Any other folder,
    /// or one another process syncs, keeps its state, is restarted all the same, and the error says why.
    pub async fn relink(&self, folder: &Path) -> Result<FolderStatus, DriverError> {
        self.restart_with(folder, Some(Fix::Relink)).await
    }

    async fn restart_with(
        &self,
        folder: &Path,
        fix: Option<Fix>,
    ) -> Result<FolderStatus, DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let _pausing = self.inner.pausing.lock().await;
        self.inner.startup_blocked.store(true, Ordering::SeqCst);
        let startup = BlockedStartup(self);
        let (link, unlinking) = {
            let mut folders = self.inner.folders.lock().unwrap();
            let found = find(&mut folders, folder)?;
            (
                found.link.clone(),
                found.unlink.is_some() || found.status.unlinking(),
            )
        };
        if unlinking && matches!(fix, Some(Fix::Relink)) {
            return Err(DriverError::UnlinkUnfinished(link.root));
        }
        let space_id = link.space_id.clone();
        loop {
            let done = (self.inner.folders.lock().unwrap().get(&space_id))
                .and_then(|folder| folder.run.as_ref().filter(|run| !run.ended()))
                .map(Running::stop);
            let Some(done) = done else { break };
            ended(done).await;
        }
        #[cfg(test)]
        let stopped = self.inner.restart_stopped.lock().unwrap().take();
        #[cfg(test)]
        if let Some(stopped) = stopped {
            let _ = stopped.reached.send(());
            let _ = stopped.proceed.await;
        }
        let fixed = match fix {
            Some(Fix::Lift(lift)) => (self.inner.paths.store(&space_id))
                .map_err(DriverError::from)
                .and_then(|path| lift_halt(&path, lift))
                .map(|()| None),
            Some(Fix::Relink) => (self.inner.paths.store(&space_id))
                .map_err(DriverError::from)
                .and_then(|path| relink(&link, &path))
                .map(Some),
            None => Ok(None),
        };
        let reset = {
            let mut folders = self.inner.folders.lock().unwrap();
            let folder = folders
                .get_mut(&space_id)
                .expect("only an unlink removes a folder, and it holds the lifecycle lock");
            if let Ok(Some(questions)) = fixed {
                let mut status = folder.status.lock().unwrap();
                status.history_id = None;
                status.last_seq = None;
                status.remote_nodes = 0;
                status.questions = questions;
                status.recovery = None;
            }
            if folder.run.as_ref().is_none_or(Running::ended) {
                folder.run = None;
                folder
                    .status
                    .restart()
                    .map_err(io_error(folder.status.path()))
            } else {
                Ok(())
            }
        };
        if let Err(error) = reset {
            if let Err(unfixed) = fixed {
                tracing::warn!(%error, "the folder's lifecycle was not saved after its fix failed");
                return Err(unfixed);
            }
            return Err(error);
        }
        drop(startup);
        let credential = self.inner.credential.lock().unwrap().clone();
        let mut folders = self.inner.folders.lock().unwrap();
        let all = roots(&folders);
        let folder = folders
            .get_mut(&space_id)
            .expect("only an unlink removes a folder, and it holds the lifecycle lock");
        if self.syncs() {
            self.start_folder(folder, credential.as_ref(), &all);
        }
        let status = folder.status.lock().unwrap().clone();
        drop(folders);
        self.changed();
        fixed.map(|_| status)
    }

    /// Whether this device's folders follow their `.gitignore` files, the `gitignore` key of `[sync]`.
    pub fn follows_gitignore(&self) -> bool {
        self.inner.rules.lock().unwrap().local.follows_gitignore()
    }

    /// Makes this device's folders follow their `.gitignore` files, or stop following them, and keeps the choice as
    /// `gitignore` under `[sync]` in the profile's configuration file, with the rest of the file as it was. Each folder
    /// whose task runs restarts with the new rules. What the change leaves out stays on disk and on the server, and
    /// what it includes again syncs (design §5.10). The file is written first, so a driver opened after a crash
    /// applies the choice. Setting the value the driver already follows restarts nothing. Returns whether the driver
    /// followed the other value before this call.
    pub async fn set_gitignore(&self, follow: bool) -> Result<bool, DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let _pausing = self.inner.pausing.lock().await;
        write_gitignore(&self.inner.paths.config_file, follow)?;
        {
            let mut rules = self.inner.rules.lock().unwrap();
            if rules.local.follows_gitignore() == follow {
                return Ok(false);
            }
            let local = rules.local.clone().with_gitignore(follow);
            *rules = DeviceRules {
                matcher: device(local.clone()),
                local,
            };
        }
        self.inner.startup_blocked.store(true, Ordering::SeqCst);
        let startup = BlockedStartup(self);
        let mut stopped = Vec::new();
        loop {
            let stopping: Vec<(String, watch::Receiver<()>)> = (self.inner.folders.lock().unwrap())
                .iter()
                .filter_map(|(space_id, folder)| {
                    let run = folder.run.as_ref().filter(|run| !run.ended())?;
                    Some((space_id.clone(), run.stop()))
                })
                .collect();
            if stopping.is_empty() {
                break;
            }
            for (space_id, done) in stopping {
                ended(done).await;
                stopped.push(space_id);
            }
        }
        drop(startup);
        let credential = self.inner.credential.lock().unwrap().clone();
        let mut folders = self.inner.folders.lock().unwrap();
        let all = roots(&folders);
        if self.syncs() {
            for space_id in &stopped {
                if let Some(folder) = folders.get_mut(space_id) {
                    self.start_folder(folder, credential.as_ref(), &all);
                }
            }
        }
        drop(folders);
        self.changed();
        Ok(true)
    }

    /// Counts the regular files in the linked folder of `space_id` that its `.gitignore` files alone leave out: those
    /// the folder excludes while it follows `.gitignore` files and includes while it does not. It reads the disk as it
    /// is now, as a scan does, whatever [`Driver::follows_gitignore`] says, and never follows a symbolic link. What it
    /// holds below a folder it cannot list, or whose rules file it cannot read, is left out of the count, and the folder
    /// is in [`GitignoreOnly::unreadable`].
    /// The space of the linked folder at `folder`.
    pub fn linked_space(&self, folder: &Path) -> Result<String, DriverError> {
        Ok(find(&mut self.inner.folders.lock().unwrap(), folder)?
            .link
            .space_id
            .clone())
    }

    pub fn gitignore_only(&self, space_id: &str) -> Result<GitignoreOnly, DriverError> {
        let link = (self.inner.folders.lock().unwrap().get(space_id))
            .map(|folder| folder.link.clone())
            .ok_or_else(|| DriverError::SpaceNotLinked(space_id.to_owned()))?;
        let local = self.inner.rules.lock().unwrap().local.clone();
        let admitted =
            match self.read_store(space_id, |store| Ok(crate::disk::admitted(&link, store))) {
                Ok(Some(admitted)) => admitted.map_err(|stop| stop.to_string()),
                Ok(None) => return Err(DriverError::SpaceNotLinked(space_id.to_owned())),
                Err(error) => Err(error.to_string()),
            };
        Ok(crate::ignore::gitignore_only(&link, admitted, local))
    }

    /// Starts the task of `folder` unless it is being unlinked or its task still runs. `all` are the roots of every
    /// linked folder.
    fn start_folder(&self, folder: &mut Folder, credential: Option<&Credential>, all: &[PathBuf]) {
        if folder.unlink.is_some()
            || folder.run.as_ref().is_some_and(|run| !run.ended())
            || folder.status.needs_restart()
        {
            return;
        }
        let linked = all
            .iter()
            .filter(|root| **root != folder.link.root)
            .cloned()
            .collect();
        match credential {
            Some(credential) => self.spawn(folder, credential, linked),
            None => halt(
                folder,
                Halted::SignedOut,
                "this device is signed out, for example after restoring a backup, which leaves out its sign-in; set it up again to sync this folder, and its files stay where they are".into(),
            ),
        }
    }

    /// Stops every folder and the update check, and waits for the folders' tasks to end, also those an earlier stop or
    /// unlink is still waiting for. State on disk is kept.
    pub async fn stop(&self) {
        self.inner.running.store(false, Ordering::SeqCst);
        self.inner.updates.stop();
        self.stop_folders().await;
    }

    async fn stop_folders(&self) {
        let stopping: Vec<_> = self
            .inner
            .folders
            .lock()
            .unwrap()
            .values()
            .filter_map(|folder| folder.run.as_ref().map(Running::stop))
            .collect();
        for done in stopping {
            ended(done).await;
        }
        for folder in self.inner.folders.lock().unwrap().values_mut() {
            if folder.run.as_ref().is_none_or(Running::ended) {
                folder.run = None;
                let mut status = folder.status.lock().unwrap();
                if status.state != FolderState::Halted {
                    status.state = FolderState::Stopped;
                }
                status.wait = None;
            }
        }
        self.changed();
    }

    /// Starts the task of `folder`. `linked` are the roots of the profile's other folders, which it must not overlap.
    pub(super) fn spawn(&self, folder: &mut Folder, credential: &Credential, linked: Vec<PathBuf>) {
        let task = match self.task(folder, credential, linked) {
            Ok(task) => task,
            Err(problem) => return halt(folder, Halted::Start, problem),
        };
        let requests = task.requests.clone();
        let (stop, stopped) = watch::channel(false);
        let (finished, done) = watch::channel(());
        folder.status.lock().unwrap().state = FolderState::Connecting;
        tokio::spawn(async move {
            task.run(stopped).await;
            drop(finished);
        });
        folder.run = Some(Running {
            stop,
            requests,
            done,
        });
    }

    pub(super) fn task(
        &self,
        folder: &Folder,
        credential: &Credential,
        linked: Vec<PathBuf>,
    ) -> Result<FolderTask, String> {
        if folder.link.device_id != credential.device_id
            || folder.link.server_url != credential.server
        {
            return Err(format!(
                "the folder was linked by device {} of {}, not by this profile's device {}; unlink it and link it again",
                folder.link.device_id, folder.link.server_url, credential.device_id
            ));
        }
        let auth = self.auth(credential).map_err(|error| error.to_string())?;
        let store = (self.inner.paths)
            .store(&folder.link.space_id)
            .map_err(|error| error.to_string())?;
        let rules = self.inner.rules.lock().unwrap();
        Ok(FolderTask {
            linked_started: folder.linked_started,
            telemetry: self.inner.telemetry.clone(),
            link: folder.link.clone(),
            device_name: credential.device_name.clone(),
            store,
            auth,
            status: folder.status.clone(),
            progress: folder.progress.clone(),
            observing: folder.observing.clone(),
            matcher: rules.matcher.clone(),
            local: rules.local.clone(),
            trash_cap: self.inner.trash_cap,
            upload_gate: self.inner.upload_gate.clone(),
            private_dirs: self.inner.paths.private_dirs(),
            linked,
            owner: Owner {
                server: credential.server.clone(),
                device_id: credential.device_id.clone(),
            },
            observer: Mutex::default(),
            changed: self.inner.changed.clone(),
            progressed: self.inner.progressed.clone(),
            totals: self.inner.totals.clone(),
            requests: Arc::default(),
            #[cfg(test)]
            observe_fault: self.inner.observe_fault.clone(),
        })
    }
}
