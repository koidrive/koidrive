use super::*;

pub(super) fn private_work(path: &Path, create: bool) -> Result<File, AppInstallError> {
    let file = directory(path, create)?;
    let metadata = file.metadata().map_err(io_error)?;
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
        return Err(failure(
            AppInstallFailure::UnsafePath,
            "Installer work root is not private",
        ));
    }
    Ok(file)
}

pub(super) fn regular_at(root: &File, name: &OsStr) -> Result<File, AppInstallError> {
    let file = File::from(
        rustix::fs::openat(
            root,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| failure(AppInstallFailure::UnsafePath, error))?,
    );
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(failure(
            AppInstallFailure::UnsafePath,
            "Unsafe transaction file",
        ));
    }
    Ok(file)
}

pub(super) fn observed(path: &Path) -> Result<Option<Identity>, AppInstallError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Ok(Some(identity(&metadata)))
        }
        Ok(_) => Err(failure(
            AppInstallFailure::UnsafePath,
            "Unexpected transaction object",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}

pub(super) fn only_entries(root: &File, allowed: &[&OsStr]) -> Result<(), AppInstallError> {
    for entry in rustix::fs::Dir::read_from(root).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        if !allowed
            .iter()
            .any(|allowed| allowed.as_encoded_bytes() == name.to_bytes())
        {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Transaction contains an unowned entry",
            ));
        }
    }
    Ok(())
}

impl Receipt {
    pub(super) fn path(&self) -> PathBuf {
        self.root.join(&self.name)
    }

    pub(super) fn data(&self) -> ReceiptData {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .0
            .clone()
    }

    fn validate_file(&self, file: &File, expected: &ReceiptData) -> Result<(), AppInstallError> {
        private_work(&self.root, false)?;
        unchanged_directory(&self.root, &self.root_handle.file)?;
        let mut current = regular_at(&self.root_handle.file, &self.name)?;
        if identity(&current.metadata().map_err(io_error)?)
            != identity(&file.metadata().map_err(io_error)?)
        {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Transaction receipt changed",
            ));
        }
        let metadata = current.metadata().map_err(io_error)?;
        if metadata.mode() & 0o077 != 0 || metadata.len() > 32 * 1024 {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Unsafe transaction receipt",
            ));
        }
        let mut bytes = Vec::new();
        (&mut current)
            .take(32 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        let observed: ReceiptData = serde_json::from_slice(&bytes)
            .map_err(|error| failure(AppInstallFailure::UnsafePath, error))?;
        if bytes.len() > 32 * 1024
            || serde_json::to_vec(&observed).map_err(io_error)?
                != serde_json::to_vec(expected).map_err(io_error)?
        {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Transaction receipt contents changed",
            ));
        }
        Ok(())
    }

    pub(super) fn save(&self, data: &ReceiptData) -> Result<(), AppInstallError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        unchanged_directory(&self.root, &self.root_handle.file)?;
        if let Some(file) = &state.1 {
            self.validate_file(file, &state.0)?;
        }
        let bytes = serde_json::to_vec(data).map_err(io_error)?;
        if bytes.len() > 32 * 1024 {
            return Err(io_error("Transaction receipt exceeds limit"));
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io_error)?
            .as_nanos();
        let temporary = OsString::from(format!(".receipt-write-{}-{nonce}", std::process::id()));
        let root = &self.root_handle.file;
        let mut file = File::from(
            rustix::fs::openat(
                root,
                &temporary,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(io_error)?,
        );
        let result: Result<(), AppInstallError> = (|| {
            file.write_all(&bytes).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            if let Some(previous) = &state.1 {
                self.validate_file(previous, &state.0)?;
            }
            rustix::fs::renameat_with(
                root,
                &temporary,
                root,
                &self.name,
                if state.1.is_none() {
                    RenameFlags::NOREPLACE
                } else {
                    RenameFlags::empty()
                },
            )
            .map_err(io_error)?;
            root.sync_all().map_err(io_error)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(root, &temporary, AtFlags::empty());
        }
        result?;
        *state = (data.clone(), Some(file));
        Ok(())
    }

    pub(super) fn remove(&self) -> Result<(), AppInstallError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.validate_file(
            state
                .1
                .as_ref()
                .ok_or_else(|| io_error("Missing receipt file"))?,
            &state.0,
        )?;
        rustix::fs::unlinkat(&self.root_handle.file, &self.name, AtFlags::empty())
            .map_err(io_error)?;
        self.root_handle.file.sync_all().map_err(io_error)
    }

    pub(super) fn validate(&self, config: &Config) -> Result<(), AppInstallError> {
        let data = self.data();
        let work_name = data
            .work
            .path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("");
        let target = Version::parse(&data.target)
            .map_err(|error| failure(AppInstallFailure::UnsafePath, error))?;
        if data.schema != 1
            || data.work_root != self.root_handle.identity
            || data.work.path.parent() != Some(config.work.as_path())
            || !work_name.starts_with("transaction-")
            || self.name != OsString::from(format!("receipt-{work_name}.json"))
            || data.image.path != data.work.path.join(format!("Koidrive-{target}-arm64.dmg"))
            || data.point.path != data.work.path.join("mount")
        {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Invalid transaction receipt ownership",
            ));
        }
        if let Some(stage) = &data.stage {
            if ![&config.system, &config.user].contains(&&stage.root.path)
                || stage.parent.path.parent() != Some(stage.root.path.as_path())
                || !stage
                    .parent
                    .path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with(".koidrive-retired-"))
            {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Invalid staging receipt ownership",
                ));
            }
            if stage.parent.identity.device != stage.root.identity.device
                || stage.bundle.device != stage.root.identity.device
            {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Cross-filesystem app receipt",
                ));
            }
            if let Some(Transfer { incoming, previous }) = &stage.transfer
                && (*incoming != stage.bundle
                    || previous
                        .as_ref()
                        .is_some_and(|old| old.identity == *incoming))
            {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Invalid exchange identities",
                ));
            }
            if let Some(previous) = stage.transfer.as_ref().and_then(Transfer::previous)
                && let Some(version) = &previous.version
            {
                Version::parse(version)
                    .map_err(|error| failure(AppInstallFailure::UnsafePath, error))?;
            }
        } else if matches!(data.lifecycle, Lifecycle::Retiring) {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Invalid retirement receipt",
            ));
        }
        let ui = data.lifecycle.ui();
        if ui.running.len() > 2
            || data.work.identity.device != data.image.identity.device
            || data.work.identity.device != data.point.identity.device
        {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Invalid lifecycle ownership",
            ));
        }
        for running in ui.running {
            if ![&config.system, &config.user].contains(&&running.root.path)
                || running.bundle.path != running.root.path.join(BUNDLE)
            {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Invalid running app receipt",
                ));
            }
            if let Some(version) = running.version {
                Version::parse(&version)
                    .map_err(|error| failure(AppInstallFailure::UnsafePath, error))?;
            }
        }
        if !matches!(
            data.lifecycle,
            Lifecycle::Preparing | Lifecycle::Aborting(_)
        ) && data
            .stage
            .as_ref()
            .and_then(|stage| stage.transfer.as_ref())
            .is_none()
        {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Lifecycle has no app transfer",
            ));
        }
        if matches!(data.lifecycle, Lifecycle::Retiring)
            && (data
                .stage
                .as_ref()
                .and_then(|stage| stage.transfer.as_ref())
                .and_then(Transfer::previous)
                .is_none()
                || !matches!(data.work_state, WorkState::Cleaned))
        {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Invalid retirement phase",
            ));
        }
        Ok(())
    }
}

pub(super) fn begin_receipt(
    config: &Config,
    work_root: &File,
    target: &Version,
) -> Result<Receipt, AppInstallError> {
    let work = tempfile::Builder::new()
        .prefix("transaction-")
        .tempdir_in(&config.work)
        .map_err(io_error)?
        .keep();
    let point = work.join("mount");
    fs::create_dir(&point).map_err(io_error)?;
    let image = work.join(format!("Koidrive-{target}-arm64.dmg"));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&image)
        .map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    let data = ReceiptData {
        schema: 1,
        work_root: identity(&work_root.metadata().map_err(io_error)?),
        work: OwnedPath::capture(work.clone())?,
        image: OwnedPath::capture(image)?,
        point: OwnedPath::capture(point)?,
        target: target.to_string(),
        work_state: WorkState::Downloading,
        lifecycle: Lifecycle::Preparing,
        stage: None,
    };
    data.work.directory()?.sync_all().map_err(io_error)?;
    let receipt = Receipt {
        root: config.work.clone(),
        name: OsString::from(format!(
            "receipt-{}.json",
            work.file_name().unwrap().to_str().unwrap()
        )),
        root_handle: PinnedDirectory::new(work_root.try_clone().map_err(io_error)?)?,
        state: Mutex::new((data.clone(), None)),
    };
    receipt.save(&data)?;
    Ok(receipt)
}
