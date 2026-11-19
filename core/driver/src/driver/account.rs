// SPDX-License-Identifier: Apache-2.0

use super::folders::{BlockedStartup, load_folders, remove_store_dir};
use super::link::{
    Unlink, forget_unreported_set_aside, owner_of, record_unreported_set_aside, roots,
    unreported_set_aside,
};
use super::*;

/// How long a sign-out waits for each call to the server before it goes on without it.
const SERVER_WAIT: Duration = Duration::from_secs(10);

/// The local versions that a folder's syncing had set aside in its `.koidrive` folder, after an unlink or a sign-out
/// removed its sync records: each one brought back into the folder under a recovered name, and each one that stayed
/// where it was because it could not be.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetAside {
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "lossy_paths"
    )]
    pub brought_back: Vec<PathBuf>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "lossy_paths"
    )]
    pub kept: Vec<PathBuf>,
}

/// Serializes `paths` as strings, with each byte sequence that is not UTF-8 replaced by U+FFFD.
pub fn lossy_paths<S: serde::Serializer>(
    paths: &[PathBuf],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(paths.iter().map(|path| path.to_string_lossy()))
}

/// What a sign-out left undone: each file or directory it could not remove, the versions syncing had set aside, and, when the server did not confirm that
/// it revoked this device, why not. Such a device can stay on the server's list until another device revokes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignedOut {
    pub not_removed: Vec<CleanupFailure>,
    pub unconfirmed: Option<Unconfirmed>,
    pub set_aside: SetAside,
}

/// A sign-out that went ahead without the server confirming that it revoked the device. The profile keeps this record
/// until it is paired again, so every later sign-out reports it too.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unconfirmed {
    pub server: String,
    pub device_id: String,
    pub reason: String,
}

impl Unconfirmed {
    /// The unconfirmed sign-out the profile at `paths` recorded, if any.
    pub fn recorded(paths: &Paths) -> Option<Self> {
        use rustix::fs::{Mode, OFlags};
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let file = File::from(
            rustix::fs::open(paths.unconfirmed_revocation(), flags, Mode::empty()).ok()?,
        );
        serde_json::from_reader(io::BufReader::new(file)).ok()
    }
}

enum AuthFailure {
    Api(ApiError),
    Unanswered,
}

impl From<ApiError> for AuthFailure {
    fn from(error: ApiError) -> Self {
        AuthFailure::Api(error)
    }
}

pub(super) fn rotating_auth(
    paths: &Paths,
    stored: &StoredCredential,
    writes: &Arc<Mutex<()>>,
) -> Result<Arc<Auth>, ApiError> {
    let files = CredentialFiles::new(
        &paths.credential(),
        stored.credential.clone(),
        writes.clone(),
    );
    Ok(Arc::new(Auth::rotating(
        stored.credential.api()?,
        stored.net(),
        Arc::new(files),
        ROTATION_PERIOD,
    )))
}

struct RevokeFailure {
    error: DriverError,
    definitive: bool,
}

async fn revoke(
    credential: &Credential,
    auth: Result<Arc<Auth>, AuthFailure>,
    confirm_last_device: bool,
    recovering: bool,
) -> Result<Option<String>, RevokeFailure> {
    let may_have_been_sent = recovering || auth.is_ok();
    let unanswered = format!("no answer within {} seconds", SERVER_WAIT.as_secs());
    let revoked = async {
        match auth {
            Ok(auth) => auth
                .api()
                .revoke_self(&auth, confirm_last_device)
                .await
                .map_err(Some),
            Err(AuthFailure::Api(error)) => Err(Some(error)),
            Err(AuthFailure::Unanswered) => Err(None),
        }
    };
    let failure = match tokio::time::timeout(SERVER_WAIT, revoked).await {
        Ok(Ok(())) => {
            tracing::info!(
                device_id = credential.device_id,
                "revoked this device on the server"
            );
            return Ok(None);
        }
        Ok(Err(None)) => (unanswered, !recovering),
        Ok(Err(Some(error))) if !confirm_last_device && error.status() == Some(409) => {
            return Err(RevokeFailure {
                error: DriverError::LastDevice,
                definitive: true,
            });
        }
        Ok(Err(Some(error))) if error.is_unauthenticated() => {
            tracing::info!(
                device_id = credential.device_id,
                "the server had already revoked this device"
            );
            return Ok(None);
        }
        Ok(Err(Some(error))) => (
            error.to_string(),
            !recovering
                && (!may_have_been_sent
                    || error
                        .status()
                        .is_some_and(|status| (400..500).contains(&status) && status != 408)),
        ),
        Err(_) => (unanswered, false),
    };
    if !confirm_last_device {
        return Err(RevokeFailure {
            error: DriverError::NotRevoked(failure.0),
            definitive: failure.1,
        });
    }
    tracing::info!(
        failure = failure.0,
        "the server did not confirm that it revoked this device"
    );
    Ok(Some(failure.0))
}

impl Driver {
    /// Pairs this profile with a server, using a code from a device that is already paired (`POST
    /// /v1/pair/complete`), and stores the credential. A profile is paired once. A pairing whose answer was lost is sent
    /// again with its first request ID when the same code is given, so the server answers it for the device it already
    /// paired. It returns what the folders of a lost credential had set aside, as [`Driver::save_credential`] does.
    pub async fn pair(
        &self,
        server: &str,
        code: &str,
        device_name: &str,
    ) -> Result<(Account, SetAside), DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.check_unpaired()?;
        if device_name.trim().is_empty() {
            return Err(DriverError::EmptyName);
        }
        let file = NewCredentialFile::create(&self.inner.paths.credential())?;
        let set_aside = self.unlink_folders_of_lost_credential().await?;
        let api = api_for(server)?;
        let pending = PendingCreate::new(&self.inner.paths, api.base(), Intent::Pair { code });
        let held = pending.hold().map_err(io_error(pending.path()))?;
        let request = PairCompleteRequest {
            code: code.to_owned(),
            device_name: device_name.to_owned(),
            platform: std::env::consts::OS.to_owned(),
            request_id: held.saved.request_id.clone(),
        };
        let account = pending
            .send(
                &held,
                || api.pair_complete(&request),
                |issued| {
                    self.adopt(
                        file,
                        api.base().to_string(),
                        issued,
                        None,
                        device_name,
                        "paired this device",
                    )
                },
            )
            .await?;
        pending_create::forget_sign_ins(&self.inner.paths);
        Ok((account, set_aside))
    }

    /// Claims a new server as its first device (`POST /v1/claim`), with the one-time code from the server's log,
    /// creating the user `user_name`, and stores the credential. Like pairing, it happens once per profile. A claim
    /// whose answer was lost is sent again with its first request ID when the same code is given, so the server answers
    /// it for the device it already created. It returns what the folders of a lost credential had set aside, as
    /// [`Driver::save_credential`] does.
    pub async fn claim(
        &self,
        server: &str,
        code: &str,
        user_name: &str,
        device_name: &str,
    ) -> Result<(Account, SetAside), DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.check_unpaired()?;
        if user_name.trim().is_empty() {
            return Err(DriverError::EmptyUserName);
        }
        if device_name.trim().is_empty() {
            return Err(DriverError::EmptyName);
        }
        let file = NewCredentialFile::create(&self.inner.paths.credential())?;
        let set_aside = self.unlink_folders_of_lost_credential().await?;
        let api = api_for(server)?;
        let pending = PendingCreate::new(&self.inner.paths, api.base(), Intent::Claim { code });
        let held = pending.hold().map_err(io_error(pending.path()))?;
        let request = ClaimRequest {
            code: code.to_owned(),
            user_name: user_name.to_owned(),
            device_name: device_name.to_owned(),
            platform: std::env::consts::OS.to_owned(),
            setup_attempt: String::new(),
            request_id: held.saved.request_id.clone(),
        };
        let account = pending
            .send(
                &held,
                || api.claim(&request),
                |issued| {
                    let user_name = if issued.user_name.is_empty() {
                        user_name.to_owned()
                    } else {
                        issued.user_name.clone()
                    };
                    self.adopt(
                        file,
                        api.base().to_string(),
                        issued,
                        Some(user_name),
                        device_name,
                        "claimed the server",
                    )
                },
            )
            .await?;
        pending_create::forget_sign_ins(&self.inner.paths);
        Ok((account, set_aside))
    }

    /// Signs this device out: stops syncing, revokes the device on the server so that a copy of the credential stops
    /// working too, unlinks every folder (their files stay) and deletes the credential, so the profile can be paired
    /// or claimed again (RFC 0009 section 5).
    ///
    /// Unless `confirm_last_device` is set, the sign-out goes on only once the server has revoked the device, or
    /// answers that another device already did. A definitive refusal keeps the credential and the folders and restores
    /// sync if it was running. It returns [`DriverError::LastDevice`] for any HTTP 409, the server's refusal to revoke the
    /// user's last active device, or [`DriverError::NotRevoked`] when the server cannot be reached or does not
    /// answer, so the person can be told that this may be their last device and confirm. While a revoke may have reached
    /// the server without a definitive answer, the account is hidden and sync stays stopped until a retry resolves it,
    /// including after a restart. With `confirm_last_device`
    /// the revocation is best effort: a sign-out without the server finishes on the device and returns why the
    /// revocation is unconfirmed, and the device stays active there until another device revokes it.
    ///
    /// Before it asks the server, it checks that every folder's owner record and store can be removed, and returns
    /// [`DriverError::UnlinkBlocked`] without changing anything when one cannot. Once the server has revoked the device,
    /// it attempts every local cleanup and returns each path that stayed. A durable cleanup record lets another
    /// sign-out finish without the credential or the stores, including after a restart.
    pub async fn sign_out(&self, confirm_last_device: bool) -> Result<SignedOut, DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.signing_out(confirm_last_device).await
    }

    /// Signs out like [`Driver::sign_out`] with `confirm_last_device`, but only while the saved credential is
    /// `device_id`'s. A caller that found the server refusing that device's credential uses it, so a credential another
    /// run saved meanwhile is kept and [`DriverError::CredentialReplaced`] names its device.
    pub async fn sign_out_device(&self, device_id: &str) -> Result<SignedOut, DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let saved = Credential::load_stored_for_revocation(&self.inner.paths.credential())?;
        if let Some(saved) = saved.as_ref().map(|stored| &stored.credential)
            && saved.device_id != device_id
        {
            return Err(DriverError::CredentialReplaced {
                device_id: saved.device_id.clone(),
            });
        }
        self.signing_out(true).await
    }

    async fn signing_out(&self, confirm_last_device: bool) -> Result<SignedOut, DriverError> {
        let was_running = self.inner.running.load(Ordering::SeqCst);
        let was_checking = self.inner.updates.is_running();
        self.inner.startup_blocked.store(true, Ordering::SeqCst);
        let _startup = BlockedStartup(self);
        self.stop().await;
        let state = &self.inner.paths.state_dir;
        let path = state.join("leaving");
        let leaving = match Leaving::load(state) {
            Ok(leaving) => leaving,
            Err(error) => {
                if was_running {
                    self.start();
                }
                if was_checking {
                    self.check_for_updates();
                }
                return Err(io_error(&path)(error));
            }
        };
        let resume = was_running
            || leaving
                .as_ref()
                .is_some_and(|leaving| leaving.intent.resume);
        let resume_updates = was_checking
            || leaving
                .as_ref()
                .is_some_and(|leaving| leaving.intent.resume_updates);
        let mut failed = Vec::new();
        let mut unconfirmed = None;
        let mut revoked_here = false;
        let mut leaving = if leaving
            .as_ref()
            .is_none_or(|leaving| leaving.phase == Phase::Prepared)
        {
            let prepared = (|| {
                if self.inner.credential.lock().unwrap().is_some() {
                    self.check_unlinks()?;
                }
                let mut intent = leaving.map(|leaving| leaving.intent).unwrap_or_default();
                intent.resume = resume;
                intent.resume_updates = resume_updates;
                for folder in self.inner.folders.lock().unwrap().values() {
                    let current = (&folder.link).into();
                    if !intent.links.contains(&current) {
                        intent.links.push(current);
                    }
                }
                for space_id in self.inner.unplaced.lock().unwrap().keys() {
                    if !intent.unplaced.contains(space_id) {
                        intent.unplaced.push(space_id.clone());
                    }
                }
                Leaving::begin(state, intent).map_err(io_error(&path))
            })();
            match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    if resume {
                        self.start();
                    }
                    if resume_updates {
                        self.check_for_updates();
                    }
                    return Err(error);
                }
            }
        } else {
            leaving.expect("an in-flight sign-out has its intent")
        };
        if leaving.phase != Phase::Committed {
            let recovering = leaving.phase == Phase::Revoking;
            let stored = Credential::load_stored_for_revocation(&self.inner.paths.credential())?;
            let mut previous_auth = None;
            revoked_here = stored.is_some();
            let revoked = match stored {
                Some(stored) => {
                    let credential = stored.credential.clone();
                    let auth = self.revocation_auth(&stored, recovering).await;
                    leaving
                        .set_phase(Phase::Revoking)
                        .map_err(io_error(&path))?;
                    previous_auth = self.inner.auth.lock().unwrap().take();
                    *self.inner.credential.lock().unwrap() = None;
                    self.changed();
                    revoke(&credential, auth, confirm_last_device, recovering)
                        .await
                        .map(|reason| {
                            reason.map(|reason| Unconfirmed {
                                server: credential.server.clone(),
                                device_id: credential.device_id.clone(),
                                reason,
                            })
                        })
                }
                None => Ok(None),
            };
            let refused = match revoked {
                Ok(failure) => {
                    unconfirmed = failure;
                    None
                }
                Err(refused) => Some(refused),
            };
            if let Some(refused) = refused {
                if refused.definitive {
                    leaving
                        .set_phase(Phase::Prepared)
                        .map_err(io_error(&path))?;
                    let stored = Credential::load_stored(&self.inner.paths.credential())?;
                    *self.inner.credential.lock().unwrap() = stored.map(|stored| stored.credential);
                    *self.inner.auth.lock().unwrap() = previous_auth;
                    if self.inner.folders.lock().unwrap().is_empty() {
                        let loaded = load_folders(&self.inner.paths, &self.inner.telemetry)?;
                        *self.inner.folders.lock().unwrap() = loaded.folders;
                        *self.inner.unplaced.lock().unwrap() = loaded.unplaced;
                    }
                    if let Err(error) = leaving.finish() {
                        tracing::warn!(path = %koidrive_fs::quoted(&path), %error, "the refused sign-out record could not be removed");
                    }
                    if resume {
                        self.start();
                    }
                    self.changed();
                }
                if resume_updates {
                    self.check_for_updates();
                }
                return Err(refused.error);
            }
            let record = self.inner.paths.unconfirmed_revocation();
            let recorded = match &unconfirmed {
                Some(unconfirmed) => koidrive_state_file::write(
                    &record,
                    &serde_json::to_vec(unconfirmed).expect("the record serializes"),
                )
                .map(|written| written.warn_unless_durable(&record)),
                None if revoked_here => {
                    fs::remove_file(&record).or_else(|error| match error.kind() {
                        io::ErrorKind::NotFound => Ok(()),
                        _ => Err(error),
                    })
                }
                None => Ok(()),
            };
            if let Err(error) = recorded {
                tracing::warn!(%error, "the record of an unconfirmed revocation could not be updated");
            }
            if let Err(error) = leaving.set_phase(Phase::Committed) {
                failed.push(CleanupFailure {
                    path: path.clone(),
                    error: error.to_string(),
                });
            }
        }
        failed.extend(self.forget_credential());
        let mut set_aside = SetAside::default();
        for link in &leaving.intent.links {
            let (done, store, earlier) = {
                let mut folders = self.inner.folders.lock().unwrap();
                folders
                    .get_mut(&link.space_id)
                    .map_or((None, None, Vec::new()), |folder| {
                        folder.unlink = Some(Unlink::Running);
                        (
                            folder.run.as_ref().map(Running::stop),
                            Some(folder.store.clone()),
                            std::mem::take(&mut folder.brought_back),
                        )
                    })
            };
            set_aside.brought_back.extend(earlier);
            if let Some(done) = done {
                ended(done).await;
            }
            let _readers_gone = store
                .as_ref()
                .map(|store| store.write().unwrap_or_else(PoisonError::into_inner));
            let kept = self.bring_back(&link.root, &link.owner, &link.space_id, |path| {
                set_aside.brought_back.push(path);
            });
            set_aside.kept.extend(kept);
            if let Err(error) =
                claim::release(&link.root, &link.owner).and_then(|released| released.durable)
                && error.kind() != io::ErrorKind::NotFound
            {
                failed.push(CleanupFailure {
                    path: claim::record(&link.root),
                    error: error.to_string(),
                });
            }
            failed.extend(self.remove_space_state(&link.space_id).err());
            if let Some(folder) = self.inner.folders.lock().unwrap().remove(&link.space_id) {
                folder.progress.lock().unwrap().clear();
            }
        }
        for space_id in &leaving.intent.unplaced {
            failed.extend(self.remove_space_state(space_id).err());
            self.inner.unplaced.lock().unwrap().remove(space_id);
        }
        if failed.is_empty()
            && let Err(error) = leaving.finish()
        {
            failed.push(CleanupFailure {
                path,
                error: error.to_string(),
            });
        }
        self.changed();
        Ok(SignedOut {
            not_removed: failed,
            unconfirmed: match revoked_here {
                true => unconfirmed,
                false => Unconfirmed::recorded(&self.inner.paths),
            },
            set_aside,
        })
    }

    fn remove_space_state(&self, space_id: &str) -> Result<(), CleanupFailure> {
        let store = self
            .inner
            .paths
            .store(space_id)
            .map_err(|error| CleanupFailure {
                path: self.inner.paths.spaces(),
                error: error.to_string(),
            })?;
        (remove_store_dir(&self.inner.paths, space_id))
            .and_then(|removed| removed.durable)
            .map_err(|error| CleanupFailure {
                path: store.parent().expect("a store has a parent").to_path_buf(),
                error: error.to_string(),
            })
    }

    async fn revocation_auth(
        &self,
        stored: &StoredCredential,
        recovering: bool,
    ) -> Result<Arc<Auth>, AuthFailure> {
        if recovering {
            let path = self.inner.paths.credential();
            let original = stored.credential.clone();
            let reload = Arc::new(move || {
                let stored = Credential::load_stored_for_revocation(&path)
                    .map_err(io::Error::other)?
                    .ok_or_else(|| io::Error::other("the recovery credential is missing"))?;
                if (
                    &stored.credential.server,
                    &stored.credential.user_id,
                    &stored.credential.device_id,
                ) != (&original.server, &original.user_id, &original.device_id)
                {
                    return Err(io::Error::other(
                        "the profile now holds another device's credential",
                    ));
                }
                Ok(stored.net())
            });
            return Ok(Arc::new(Auth::reloading(
                stored.credential.api()?,
                stored.credential.net(),
                reload,
            )));
        }
        let auth = self.auth(&stored.credential)?;
        let token = tokio::time::timeout(SERVER_WAIT, auth.access_token())
            .await
            .map_err(|_| AuthFailure::Unanswered)??;
        Ok(Arc::new(
            Auth::new(stored.credential.api()?, auth.credentials()).with_token(token),
        ))
    }

    fn forget_credential(&self) -> Vec<CleanupFailure> {
        let path = self.inner.paths.credential();
        let failed = {
            let _writes = self.inner.credential_writes.lock().unwrap();
            [next_path(&path), path]
                .into_iter()
                .filter_map(|path| match fs::remove_file(&path) {
                    Ok(()) => None,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => Some(CleanupFailure {
                        path,
                        error: error.to_string(),
                    }),
                })
                .collect()
        };
        *self.inner.auth.lock().unwrap() = None;
        *self.inner.credential.lock().unwrap() = None;
        self.inner.running.store(false, Ordering::SeqCst);
        tracing::info!("signed out");
        self.changed();
        failed
    }

    fn check_unlinks(&self) -> Result<(), DriverError> {
        let links: Vec<Link> = (self.inner.folders.lock().unwrap().values())
            .map(|folder| folder.link.clone())
            .collect();
        let paths = &self.inner.paths;
        let mut dirs = vec![paths.state_dir.clone(), paths.spaces()];
        for link in &links {
            match claim::check_release(&link.root, &owner_of(link)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(DriverError::UnlinkBlocked {
                        path: link.root.clone(),
                        error,
                    });
                }
            }
            dirs.extend(paths.store(&link.space_id)?.parent().map(Path::to_path_buf));
        }
        for space_id in self.inner.unplaced.lock().unwrap().keys() {
            dirs.extend(paths.store(space_id)?.parent().map(Path::to_path_buf));
        }
        for dir in dirs {
            let access = rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK;
            match rustix::fs::access(&dir, access) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                Err(error) => {
                    return Err(DriverError::UnlinkBlocked {
                        path: dir,
                        error: error.into(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Unlinks the folders of a profile that has no credential, before it pairs or claims. Such folders were linked by
    /// a device whose credential is gone, typically on a Mac restored from a backup, which leaves the credential out.
    /// Their stores and owner records belong to that device, so the new device links the folders again; their files
    /// stay.
    async fn unlink_folders_of_lost_credential(&self) -> Result<SetAside, DriverError> {
        let state = &self.inner.paths.state_dir;
        let record = self.inner.paths.unreported_set_aside();
        let mut set_aside = unreported_set_aside(&record);
        let mut add = |more: SetAside| {
            if more != SetAside::default() {
                set_aside.brought_back.extend(more.brought_back);
                set_aside.kept.extend(more.kept);
                record_unreported_set_aside(&record, &set_aside);
            }
        };
        if Leaving::load(state)
            .map_err(io_error(&state.join("leaving")))?
            .is_some()
        {
            let left = self.signing_out(false).await?;
            add(left.set_aside);
            if let Some(failure) = left.not_removed.into_iter().next() {
                return Err(io_error(&failure.path)(io::Error::other(failure.error)));
            }
        }
        let roots = roots(&self.inner.folders.lock().unwrap());
        for root in roots {
            add(self.unlink_folder(&root).await?.1);
        }
        let unplaced: Vec<_> = self
            .inner
            .unplaced
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for space_id in unplaced {
            self.forget_unplaced(&space_id)?;
        }
        Ok(set_aside)
    }

    pub(super) fn check_unpaired(&self) -> Result<(), DriverError> {
        let state = &self.inner.paths.state_dir;
        if crate::leave::phase(state).map_err(io_error(&state.join("leaving")))?
            == Some(Phase::Revoking)
        {
            return Err(CredentialError::SignOutPending.into());
        }
        match self.inner.credential.lock().unwrap().as_ref() {
            Some(credential) => Err(DriverError::AlreadyPaired {
                device_id: credential.device_id.clone(),
            }),
            None => Ok(()),
        }
    }

    fn adopt(
        &self,
        file: NewCredentialFile,
        server: String,
        issued: DeviceCredentials,
        user_name: Option<String>,
        device_name: &str,
        done: &str,
    ) -> Result<Account, DriverError> {
        let account = self.store_credential(
            Credential {
                server,
                user_id: issued.user_id,
                user_name,
                device_id: issued.device_id,
                device_name: device_name.to_owned(),
                refresh_credential: issued.refresh_credential,
            },
            |credential| {
                file.write(credential)?
                    .warn_unless_durable(&self.inner.paths.credential());
                Ok(())
            },
        )?;
        tracing::info!(
            device_id = account.device_id,
            server = account.server,
            "{done}"
        );
        Ok(account)
    }

    /// Stores a credential this profile was issued elsewhere, such as by `koi setup`'s claim, the same way
    /// [`Driver::pair`] and [`Driver::claim`] store theirs, unless another one was stored meanwhile. Like them, it first
    /// unlinks the folders of a credential that was lost, and returns what those folders had set aside. The file is
    /// written before this returns, so a caller can clean up after it. Like every change to the credential, it waits for
    /// the lifecycle lock, so a sign-out that checked the slot finishes before a new credential appears.
    pub async fn save_credential(
        &self,
        credential: Credential,
    ) -> Result<(Account, SetAside), DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.check_unpaired()?;
        let set_aside = self.unlink_folders_of_lost_credential().await?;
        let path = self.inner.paths.credential();
        let account = self.store_credential(credential, |credential| credential.save_new(&path))?;
        Ok((account, set_aside))
    }

    /// Stores `credential` with `write` unless the slot holds one, while the caller holds the lifecycle lock.
    fn store_credential(
        &self,
        credential: Credential,
        write: impl FnOnce(&Credential) -> Result<(), CredentialError>,
    ) -> Result<Account, DriverError> {
        let mut slot = self.inner.credential.lock().unwrap();
        if let Some(existing) = slot.as_ref() {
            return Err(DriverError::AlreadyPaired {
                device_id: existing.device_id.clone(),
            });
        }
        write(&credential)?;
        if let Err(error) = fs::remove_file(self.inner.paths.unconfirmed_revocation())
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "a sign-out of an earlier device stays reported as unconfirmed");
        }
        forget_unreported_set_aside(&self.inner.paths);
        let account = credential.account();
        let stored = StoredCredential {
            credential: credential.clone(),
            staged: None,
            changed_at: SystemTime::now(),
        };
        *self.inner.auth.lock().unwrap() =
            rotating_auth(&self.inner.paths, &stored, &self.inner.credential_writes).ok();
        *slot = Some(credential);
        drop(slot);
        self.changed();
        Ok(account)
    }

    pub(super) fn auth(&self, credential: &Credential) -> Result<Arc<Auth>, ApiError> {
        let mut slot = self.inner.auth.lock().unwrap();
        if let Some(auth) = slot.as_ref() {
            return Ok(auth.clone());
        }
        let stored = Credential::load_stored(&self.inner.paths.credential())
            .map_err(|error| ApiError::Credential(io::Error::other(error)))?
            .unwrap_or_else(|| StoredCredential {
                credential: credential.clone(),
                staged: None,
                changed_at: SystemTime::now(),
            });
        let auth = rotating_auth(&self.inner.paths, &stored, &self.inner.credential_writes)?;
        *slot = Some(auth.clone());
        Ok(auth)
    }

    /// The profile's one [`Auth`], for calls to the server that the driver does not make itself. They present the
    /// credential that the last rotation left current, like every caller in this process.
    pub fn signed_in(&self) -> Result<Arc<Auth>, DriverError> {
        let credential = self.credential()?;
        Ok(self.auth(&credential)?)
    }

    pub(super) fn reading_auth(&self) -> Result<Arc<Auth>, DriverError> {
        let credential = self.credential()?;
        let path = self.inner.paths.credential();
        let original = credential.clone();
        let reload = Arc::new(move || {
            let stored = Credential::load_stored(&path)
                .map_err(io::Error::other)?
                .ok_or_else(|| io::Error::other("the profile no longer holds a credential"))?;
            if (
                &stored.credential.server,
                &stored.credential.user_id,
                &stored.credential.device_id,
            ) != (&original.server, &original.user_id, &original.device_id)
            {
                return Err(io::Error::other(
                    "the profile now holds another device's credential",
                ));
            }
            Ok(stored.net())
        });
        Ok(Arc::new(Auth::reloading(
            credential.api()?,
            credential.net(),
            reload,
        )))
    }

    pub(super) fn credential(&self) -> Result<Credential, DriverError> {
        self.inner
            .credential
            .lock()
            .unwrap()
            .clone()
            .ok_or(DriverError::NotPaired)
    }
}
