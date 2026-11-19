// SPDX-License-Identifier: Apache-2.0

use super::folders::{folder_status, halt, remove_store_dir};
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LinkApproval {
    pub directory: crate::DirectoryIdentity,
    pub accept_existing_files: bool,
}

impl LinkApproval {
    fn check_directory(&self, root: &Path) -> Result<(), DriverError> {
        let current = fs::metadata(root).map_err(io_error(root))?;
        if self.directory != crate::DirectoryIdentity::of(&current) {
            return Err(DriverError::FolderApprovalChanged(root.to_owned()));
        }
        Ok(())
    }
}

/// What [`Driver::link`] did, with the folder's status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Linked {
    /// The folder is linked now.
    New(FolderStatus),
    /// The folder was already linked to the space, and nothing changed.
    Already(FolderStatus),
}

pub(super) const UNLINK_BROUGHT_BACK: &str = "unlink-brought-back.json";

#[derive(Default, Serialize, Deserialize)]
struct Unreported {
    brought_back: Vec<Vec<u8>>,
    kept: Vec<Vec<u8>>,
}

pub(super) fn unreported_set_aside(record: &Path) -> SetAside {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::ffi::OsStringExt;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let recorded = (rustix::fs::open(record, flags, Mode::empty()).ok())
        .and_then(|file| {
            serde_json::from_reader::<_, Unreported>(io::BufReader::new(File::from(file))).ok()
        })
        .unwrap_or_default();
    let paths = |paths: Vec<Vec<u8>>| {
        (paths.into_iter())
            .map(|path| PathBuf::from(std::ffi::OsString::from_vec(path)))
            .collect()
    };
    SetAside {
        brought_back: paths(recorded.brought_back),
        kept: paths(recorded.kept),
    }
}

pub(super) fn record_unreported_set_aside(record: &Path, set_aside: &SetAside) {
    use std::os::unix::ffi::OsStrExt;
    let bytes = |paths: &[PathBuf]| {
        (paths.iter())
            .map(|path| path.as_os_str().as_bytes().to_vec())
            .collect()
    };
    let unreported = Unreported {
        brought_back: bytes(&set_aside.brought_back),
        kept: bytes(&set_aside.kept),
    };
    let bytes = serde_json::to_vec(&unreported).expect("the record serializes");
    if let Err(error) = koidrive_state_file::write(record, &bytes)
        .map(|written| written.warn_unless_durable(record))
    {
        tracing::warn!(%error, "what the folders of a lost credential set aside is reported only by this process");
    }
}

pub(super) fn forget_unreported_set_aside(paths: &Paths) {
    if let Err(error) = fs::remove_file(paths.unreported_set_aside())
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "what the folders of a lost credential set aside could not be marked reported");
    }
}

pub(super) fn recorded_brought_back(store: &Path) -> Vec<PathBuf> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::ffi::OsStringExt;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    (rustix::fs::open(
        store.with_file_name(UNLINK_BROUGHT_BACK),
        flags,
        Mode::empty(),
    )
    .ok())
    .and_then(|file| {
        serde_json::from_reader::<_, Vec<Vec<u8>>>(io::BufReader::new(File::from(file))).ok()
    })
    .unwrap_or_default()
    .into_iter()
    .map(|path| PathBuf::from(std::ffi::OsString::from_vec(path)))
    .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Unlink {
    Running,
    /// Its caller stopped waiting before it finished. Another unlink or a sign-out finishes it.
    Abandoned,
}

struct Unlinking<'a> {
    driver: &'a Driver,
    space_id: String,
}

impl Drop for Unlinking<'_> {
    fn drop(&mut self) {
        let mut folders = self.driver.inner.folders.lock().unwrap();
        let Some(folder) = folders.get_mut(&self.space_id) else {
            return;
        };
        if folder.unlink == Some(Unlink::Running) {
            folder.unlink = Some(Unlink::Abandoned);
            folder.status.halt(
                Halted::Unlink,
                crate::lifecycle::UNFINISHED_UNLINK.into(),
                crate::status::Remedy::Unlink,
            );
            drop(folders);
            self.driver.changed();
        }
    }
}

pub(super) fn owner_of(link: &Link) -> Owner {
    Owner {
        server: link.server_url.clone(),
        device_id: link.device_id.clone(),
    }
}

pub(super) fn roots(folders: &BTreeMap<String, Folder>) -> Vec<PathBuf> {
    folders
        .values()
        .map(|folder| folder.link.root.clone())
        .collect()
}

pub(super) fn held_aside(root: &Path) -> Vec<PathBuf> {
    let private = root.join(koidrive_fs::PRIVATE_DIR);
    if !fs::symlink_metadata(&private).is_ok_and(|meta| meta.is_dir()) {
        return Vec::new();
    }
    let mut held = Vec::new();
    for dir in [koidrive_fs::PARKED_DIR, koidrive_fs::QUARANTINE_DIR].map(|dir| private.join(dir)) {
        let listed = fs::symlink_metadata(&dir).and_then(|meta| match meta.is_dir() {
            true => fs::read_dir(&dir)?.map(|entry| Ok(entry?.path())).collect(),
            false => Ok(vec![dir.clone()]),
        });
        match listed {
            Ok(entries) => held.extend(entries),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(_) => held.push(dir),
        }
    }
    held.sort();
    held
}

/// Refuses a folder that holds or sits inside one of the profile's private directories or another linked folder.
/// Folders are compared by the identity of each folder and its ancestors as well as by path, so a symbolic link or a
/// bind mount does not hide an overlap. Linking checks this, and so does each folder task before it observes the
/// folder or connects, because a profile can be moved after its folders were linked.
pub(crate) fn check_placement(
    root: &Path,
    private_dirs: &[PathBuf],
    linked: &[PathBuf],
) -> Result<(), DriverError> {
    let canonical = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = canonical(root);
    for dir in private_dirs {
        let dir = canonical(dir);
        if holds(&root, &dir) || holds(&dir, &root) {
            return Err(DriverError::FolderHoldsProfile(dir));
        }
    }
    for other in linked {
        let canonical = canonical(other);
        if holds(&root, &canonical) || holds(&canonical, &root) {
            return Err(DriverError::FolderOverlaps(other.clone()));
        }
    }
    Ok(())
}

fn holds(outer: &Path, inner: &Path) -> bool {
    if inner.starts_with(outer) {
        return true;
    }
    let Ok(outer) = fs::metadata(outer) else {
        return false;
    };
    inner
        .ancestors()
        .filter_map(|dir| fs::metadata(dir).ok())
        .any(|dir| dir.dev() == outer.dev() && dir.ino() == outer.ino())
}

pub(super) fn find<'a>(
    folders: &'a mut BTreeMap<String, Folder>,
    folder: &Path,
) -> Result<&'a mut Folder, DriverError> {
    let wanted = absolute(folder);
    let canonical = fs::canonicalize(folder).ok();
    folders
        .values_mut()
        .find(|linked| linked.link.root == wanted || Some(&linked.link.root) == canonical.as_ref())
        .ok_or_else(|| DriverError::NotLinked(folder.to_path_buf()))
}

pub(super) fn resolve_space(
    spaces: Vec<SpaceInfo>,
    wanted: &str,
) -> Result<SpaceInfo, DriverError> {
    if let Some(space) = spaces.iter().find(|space| space.space_id == wanted) {
        return Ok(space.clone());
    }
    let mut named: Vec<SpaceInfo> = spaces
        .into_iter()
        .filter(|space| space.name == wanted)
        .collect();
    match named.len() {
        0 => Err(DriverError::SpaceNotFound(wanted.to_owned())),
        1 => Ok(named.remove(0)),
        _ => Err(DriverError::SpaceAmbiguous {
            name: wanted.to_owned(),
            ids: named.into_iter().map(|space| space.space_id).collect(),
        }),
    }
}

impl Driver {
    /// Links `folder` to the space with id or name `space`: checks the folder, writes its root marker, and creates
    /// its store. The folder starts syncing at once if the driver is running. A folder already linked to that space
    /// stays as it is, and one linked to another space is refused.
    pub async fn link(&self, folder: &Path, space: &str) -> Result<Linked, DriverError> {
        self.link_approved(folder, space, None).await
    }

    pub async fn link_approved(
        &self,
        folder: &Path,
        space: &str,
        approval: Option<LinkApproval>,
    ) -> Result<Linked, DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let credential = self.credential()?;
        let root = fs::canonicalize(folder).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => DriverError::FolderNotFound(folder.to_path_buf()),
            _ => io_error(folder)(error),
        })?;
        if !root.is_dir() {
            return Err(DriverError::NotAFolder(root));
        }
        if let Some(approval) = approval {
            approval.check_directory(&root)?;
        }
        let here = (self.inner.folders.lock().unwrap().values())
            .find(|linked| linked.link.root == root)
            .map(|linked| {
                let status = linked.status.lock().unwrap().clone();
                (linked.link.clone(), status, linked.unlink.is_some())
            });
        if let Some((link, status, unlinking)) = here {
            if unlinking {
                return Err(DriverError::UnlinkUnfinished(root));
            }
            if link.device_id != credential.device_id || link.server_url != credential.server {
                return Err(DriverError::LinkedByOtherDevice {
                    device_id: link.device_id,
                    server: link.server_url,
                });
            }
            let space_id = link.space_id;
            let same = space == space_id || {
                let auth = self.auth(&credential)?;
                resolve_space(auth.api().list_spaces(&auth).await?.spaces, space)?.space_id
                    == space_id
            };
            if let Some(approval) = approval {
                approval.check_directory(&root)?;
            }
            return match same {
                true => Ok(Linked::Already(status)),
                false => Err(DriverError::FolderOverlaps(root)),
            };
        }
        self.check_placement(&root, &self.inner.folders.lock().unwrap())?;
        if !probe_no_replace(&root).map_err(io_error(&root))? {
            return Err(DriverError::UnsupportedFileSystem);
        }
        let owner = Owner {
            server: credential.server.clone(),
            device_id: credential.device_id.clone(),
        };
        let lock = claim::lock(&root)
            .map_err(io_error(&root))?
            .ok_or_else(|| DriverError::FolderInUse(root.clone()))?;
        match claim::owner(&root).map_err(io_error(&root))? {
            Some(other) if other != owner => {
                return Err(DriverError::FolderOfOtherDevice {
                    device_id: other.device_id,
                    server: other.server,
                });
            }
            _ => {}
        }
        let auth = self.auth(&credential)?;
        let space = resolve_space(auth.api().list_spaces(&auth).await?.spaces, space)?;
        if self
            .inner
            .unplaced
            .lock()
            .unwrap()
            .contains_key(&space.space_id)
        {
            return Err(DriverError::SpaceUnplaced(space.space_id));
        }

        let mut folders = self.inner.folders.lock().unwrap();
        self.check_placement(&root, &folders)?;
        if let Some(linked) = folders.get(&space.space_id) {
            return Err(DriverError::SpaceAlreadyLinked(linked.link.root.clone()));
        }
        if let Some(approval) = approval {
            approval.check_directory(&root)?;
            let unapproved_files = !approval.accept_existing_files
                && has_sync_entries(&root).map_err(io_error(&root))?;
            if unapproved_files || !claim::still_locked(&lock, &root) {
                return Err(DriverError::FolderApprovalChanged(root));
            }
        }
        let found_marker = match read_marker(&root).map_err(io_error(&root))? {
            Some(marker) if marker.space_id != space.space_id => {
                return Err(DriverError::MarkerOfOtherSpace(marker.space_id));
            }
            Some(_) => true,
            None => false,
        };
        if !found_marker {
            write_marker(
                &root,
                &Marker {
                    space_id: space.space_id.clone(),
                    effects: 0,
                    token: None,
                },
            )
            .map_err(io_error(&root))?;
        }
        claim::set_owner(&root, &owner).map_err(io_error(&root))?;
        let linked_started = crate::timing::Time::now();
        let found = fs::File::open(&root)
            .and_then(|dir| root_at(dir.as_fd()))
            .map_err(io_error(&root))?;
        let link = Link {
            space_id: space.space_id.clone(),
            space_name: space.name.clone(),
            root_dev: found.dev,
            root_volume: found.volume,
            root,
            server_url: credential.server.clone(),
            device_id: credential.device_id.clone(),
            linked_at_ms: linked_started.unix_ms,
            found_marker,
        };
        let store_path = self.inner.paths.store(&link.space_id)?;
        let store_dir = store_path.parent().expect("a store path has a parent");
        self.inner
            .paths
            .create_state_dir(store_dir)
            .map_err(io_error(store_dir))?;
        Store::open(&store_path)
            .and_then(|mut store| store.transaction(|tx| tx.set_link(&link)))
            .map_err(store_error(&store_path))?;
        if let Err(error) = crate::link_copy::write(&store_path, &link) {
            tracing::warn!(path = %koidrive_fs::quoted(&crate::link_copy::path(&store_path)), %error, "the copy of a folder's link could not be saved");
        }
        crate::timing::Timing::initialize(&store_path, link.linked_at_ms, &self.inner.telemetry);
        tracing::info!(space_id = link.space_id, path = %koidrive_fs::quoted(&link.root), "linked a folder");
        let status = Arc::new(crate::lifecycle::Lifecycle::open(
            &store_path,
            folder_status(&link, None, 0, 0),
            self.inner.telemetry.clone(),
        ));
        status.settle_unreadable();
        let linked = roots(&folders);
        let mut folder = Folder {
            linked_started: Some(linked_started),
            link,
            status: status.clone(),
            run: None,
            unlink: None,
            store: Arc::default(),
            progress: Arc::default(),
            observing: Arc::default(),
            brought_back: Vec::new(),
        };
        drop(lock);
        if self.syncs() && !folder.status.needs_restart() {
            self.spawn(&mut folder, &credential, linked);
        }
        folders.insert(space.space_id, folder);
        drop(folders);
        self.changed();
        let status = status.lock().unwrap().clone();
        Ok(Linked::New(status))
    }

    /// Stops syncing the folder at `folder` and forgets its sync state. The files stay, and so does the folder's
    /// `.koidrive` directory with its local trash. Local versions that syncing set aside there are first brought back
    /// into the folder under recovered names, as recovery names them; the ones that cannot be stay and are returned as
    /// kept. If the owner record or the store cannot be removed, the folder stays linked and halted with the error, so
    /// unlinking it again finishes the job.
    pub async fn unlink(&self, folder: &Path) -> Result<(Link, SetAside), DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.unlink_folder(folder).await
    }

    pub(super) async fn unlink_folder(
        &self,
        folder: &Path,
    ) -> Result<(Link, SetAside), DriverError> {
        let (link, done, store, earlier) = {
            let mut folders = self.inner.folders.lock().unwrap();
            let found = find(&mut folders, folder)?;
            if let Err(error) = found.status.begin_unlink() {
                let error = io_error(found.status.path())(error);
                if found.run.as_ref().is_none_or(Running::ended) {
                    halt(
                        found,
                        Halted::Io,
                        format!("the folder could not be unlinked: {error}"),
                    );
                }
                drop(folders);
                self.changed();
                return Err(error);
            }
            found.unlink = Some(Unlink::Running);
            let done = found.run.as_ref().map(Running::stop);
            (
                found.link.clone(),
                done,
                found.store.clone(),
                std::mem::take(&mut found.brought_back),
            )
        };
        let _unlinking = Unlinking {
            driver: self,
            space_id: link.space_id.clone(),
        };
        if let Some(done) = done {
            ended(done).await;
        }
        let (set_aside, forgotten) = {
            let _readers_gone = store.write().unwrap_or_else(PoisonError::into_inner);
            self.forget(&link, earlier)
        };
        let mut folders = self.inner.folders.lock().unwrap();
        match forgotten {
            Ok(()) => {
                if let Some(folder) = folders.remove(&link.space_id) {
                    folder.progress.lock().unwrap().clear();
                }
                tracing::info!(space_id = link.space_id, path = %koidrive_fs::quoted(&link.root), "unlinked a folder");
            }
            Err(ref error) => {
                if let Some(folder) = folders.get_mut(&link.space_id) {
                    folder.brought_back.clone_from(&set_aside.brought_back);
                    folder.run = None;
                    folder.unlink = None;
                    halt(
                        folder,
                        Halted::Unlink,
                        format!("the folder could not be unlinked: {error}"),
                    );
                }
            }
        }
        drop(folders);
        self.changed();
        match forgotten {
            Ok(()) => Ok((link, set_aside)),
            Err(error) if set_aside.brought_back.is_empty() => Err(error),
            Err(error) => Err(DriverError::UnlinkFailedAfterBringingBack {
                error: Box::new(error),
                brought_back: set_aside.brought_back,
            }),
        }
    }

    /// Unlinks the folder that syncs `space_id` as [`Driver::unlink`] does, and returns its link. A folder whose sync
    /// database and copy of its link can't be read has no known path: only its state on this device is removed, and
    /// this returns `None`.
    pub async fn unlink_space(
        &self,
        space_id: &str,
    ) -> Result<Option<(Link, SetAside)>, DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.inner.paths.store(space_id)?;
        let root = (self.inner.folders.lock().unwrap().get(space_id))
            .map(|folder| folder.link.root.clone());
        if let Some(root) = root {
            return self.unlink_folder(&root).await.map(Some);
        }
        if !self.inner.unplaced.lock().unwrap().contains_key(space_id) {
            return Err(DriverError::SpaceNotLinked(space_id.to_owned()));
        }
        self.forget_unplaced(space_id)?;
        self.changed();
        Ok(None)
    }

    pub(super) fn forget_unplaced(&self, space_id: &str) -> Result<(), DriverError> {
        let store_dir = (self.inner.paths.store(space_id)?.parent())
            .expect("a store path has a parent")
            .to_path_buf();
        remove_store_dir(&self.inner.paths, space_id)
            .map_err(io_error(&store_dir))?
            .warn_unless_durable(&store_dir);
        self.inner.unplaced.lock().unwrap().remove(space_id);
        tracing::info!(space_id, "unlinked a folder whose path is unknown");
        Ok(())
    }

    fn forget(
        &self,
        link: &Link,
        mut brought_back: Vec<PathBuf>,
    ) -> (SetAside, Result<(), DriverError>) {
        let owner = Owner {
            server: link.server_url.clone(),
            device_id: link.device_id.clone(),
        };
        let store = self
            .inner
            .paths
            .store(&link.space_id)
            .map_err(io::Error::other);
        let kept = self.bring_back(&link.root, &owner, &link.space_id, |path| {
            brought_back.push(path);
            let recorded = match &store {
                Ok(store) => {
                    use std::os::unix::ffi::OsStrExt;
                    let bytes: Vec<&[u8]> =
                        (brought_back.iter()).map(|path| path.as_os_str().as_bytes()).collect();
                    serde_json::to_vec(&bytes)
                }
                .map_err(io::Error::other)
                    .and_then(|bytes| {
                        let record = store.with_file_name(UNLINK_BROUGHT_BACK);
                        koidrive_state_file::write(&record, &bytes)
                            .map(|written| written.warn_unless_durable(&record))
                    }),
                Err(error) => Err(io::Error::other(error.to_string())),
            };
            if let Err(error) = recorded {
                tracing::warn!(space_id = link.space_id, %error, "a version brought back into the folder could not be recorded");
            }
        });
        let set_aside = SetAside { brought_back, kept };
        (set_aside, self.release_and_remove(link, &owner))
    }

    fn release_and_remove(&self, link: &Link, owner: &Owner) -> Result<(), DriverError> {
        let store_dir = self
            .inner
            .paths
            .store(&link.space_id)?
            .parent()
            .expect("a store path has a parent")
            .to_path_buf();
        let spaces = self.inner.paths.spaces();
        rustix::fs::accessat(
            rustix::fs::CWD,
            &spaces,
            rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK,
            rustix::fs::AtFlags::EACCESS,
        )
        .map_err(|error| io_error(&spaces)(error.into()))?;
        match claim::release(&link.root, owner) {
            Ok(released) => released.durable.map_err(io_error(&link.root))?,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(io_error(&link.root)(error)),
        }
        remove_store_dir(&self.inner.paths, &link.space_id)
            .map_err(io_error(&store_dir))?
            .warn_unless_durable(&store_dir);
        Ok(())
    }

    pub(super) fn bring_back(
        &self,
        root: &Path,
        owner: &Owner,
        space_id: &str,
        mut moved: impl FnMut(PathBuf),
    ) -> Vec<PathBuf> {
        if held_aside(root).is_empty() {
            return Vec::new();
        }
        match self.bring_back_held(root, owner, space_id, &mut moved) {
            Ok(true) => {}
            Ok(false) => return Vec::new(),
            Err(error) => {
                tracing::warn!(space_id, %error, "what the folder set aside stays in its .koidrive folder");
            }
        }
        held_aside(root)
    }

    fn bring_back_held(
        &self,
        root: &Path,
        owner: &Owner,
        space_id: &str,
        moved: &mut impl FnMut(PathBuf),
    ) -> io::Result<bool> {
        let other = || Ok::<_, io::Error>(claim::owner(root)?.is_some_and(|found| found != *owner));
        if other()? {
            return Ok(false);
        }
        let lock = claim::lock(root)?
            .ok_or_else(|| io::Error::other("another Koidrive process syncs the folder"))?;
        if other()? {
            return Ok(false);
        }
        let store = self.inner.paths.store(space_id).map_err(io::Error::other)?;
        let (link, engine) = Store::open_existing(&store)
            .and_then(|mut store| {
                store.transaction(|tx| Ok((tx.link()?, crate::engine::Engine::load(tx, space_id)?)))
            })
            .map_err(io::Error::other)?;
        let link =
            link.ok_or_else(|| io::Error::other("the folder's sync records name no folder"))?;
        let space = koidrive_fs::Space::bind(OwnedFd::from(lock.try_clone()?), root)?;
        (link.root_at().admits(&space.root_at()?)).map_err(io::Error::other)?;
        let name = |held: &OsStr, found| crate::disk::held_name(&engine.trees, held, found);
        space.bring_back_each(name, SystemTime::now(), |result| match result {
            Ok(path) => moved(root.join(path)),
            Err(error) => {
                tracing::warn!(space_id, %error, "a version the folder set aside stays in its .koidrive folder");
            }
        })?;
        Ok(true)
    }

    pub(super) fn check_placement(
        &self,
        root: &Path,
        folders: &BTreeMap<String, Folder>,
    ) -> Result<(), DriverError> {
        check_placement(root, &self.inner.paths.private_dirs(), &roots(folders))
    }
}
