use super::*;

async fn cleanup_work(
    receipt: &Receipt,
    config: &Config,
    control: &Control,
) -> Result<(), AppInstallError> {
    let mut data = receipt.data();
    if observed(&data.work.path)?.is_none() {
        if !data.work_state.cleaning() {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Transaction work disappeared",
            ));
        }
        if data.work_state.attached() {
            let mount = OwnedMount {
                point: data.point.path.clone(),
                image: data.image.path.clone(),
            };
            if !matches!(mount.state(config, control).await?, MountState::Absent) {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Missing work still has a pending mount",
                ));
            }
        }
        if !matches!(data.work_state, WorkState::Cleaned) {
            data.work_state = WorkState::Cleaned;
            receipt.save(&data)?;
        }
        return Ok(());
    }
    let work = data.work.directory()?;
    if matches!(data.work_state, WorkState::Cleaned) {
        return Err(failure(
            AppInstallFailure::ConcurrentChange,
            "Cleaned transaction work reappeared",
        ));
    }
    only_entries(
        &work,
        &[OsStr::new("mount"), data.image.path.file_name().unwrap()],
    )?;
    let image = match regular_at(&work, data.image.path.file_name().unwrap()) {
        Ok(file) => {
            if identity(&file.metadata().map_err(io_error)?) != data.image.identity {
                return Err(failure(
                    AppInstallFailure::ConcurrentChange,
                    "Transaction image changed",
                ));
            }
            Some(file)
        }
        Err(_error)
            if data.work_state.cleaning()
                && fs::symlink_metadata(&data.image.path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let mount = OwnedMount {
        point: data.point.path.clone(),
        image: data.image.path.clone(),
    };
    if data.work_state.attached() {
        match mount.state(config, control).await? {
            MountState::Owned if image.is_none() => {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Mounted transaction image disappeared",
                ));
            }
            MountState::Foreign => {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Foreign image occupies pending mount",
                ));
            }
            MountState::Owned => {
                let metadata = fs::symlink_metadata(&data.point.path).map_err(io_error)?;
                if !metadata.is_dir()
                    || metadata.file_type().is_symlink()
                    || (metadata.dev() == data.point.identity.device
                        && identity(&metadata) != data.point.identity)
                {
                    return Err(failure(
                        AppInstallFailure::ConcurrentChange,
                        "Pending mountpoint changed",
                    ));
                }
            }
            _ => (),
        }
    }
    data.work_state = WorkState::Cleaning {
        attached: data.work_state.attached(),
    };
    receipt.save(&data)?;
    unchanged_directory(&data.work.path, &work)?;
    if data.work_state.attached() && image.is_some() {
        mount.cleanup(config, data.image.identity, control).await?;
    }
    unchanged_directory(&data.work.path, &work)?;
    if observed(&data.point.path)?.is_some() {
        let point = data.point.directory()?;
        only_entries(&point, &[])?;
        rustix::fs::unlinkat(&work, "mount", AtFlags::REMOVEDIR).map_err(io_error)?;
    }
    if let Some(file) = image {
        let current = regular_at(&work, data.image.path.file_name().unwrap())?;
        if identity(&current.metadata().map_err(io_error)?)
            != identity(&file.metadata().map_err(io_error)?)
        {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Image changed during cleanup",
            ));
        }
        rustix::fs::unlinkat(
            &work,
            data.image.path.file_name().unwrap(),
            AtFlags::empty(),
        )
        .map_err(io_error)?;
    }
    only_entries(&work, &[])?;
    unchanged_directory(&data.work.path, &work)?;
    unchanged_directory(&receipt.root, &receipt.root_handle.file)?;
    rustix::fs::unlinkat(
        &receipt.root_handle.file,
        data.work.path.file_name().unwrap(),
        AtFlags::REMOVEDIR,
    )
    .map_err(io_error)?;
    receipt.root_handle.file.sync_all().map_err(io_error)?;
    data.work_state = WorkState::Cleaned;
    receipt.save(&data)
}

fn cleanup_stage(stage: &Stage, control: &Control) -> Result<(), AppInstallError> {
    let root = stage.root.directory()?;
    let parent = stage.parent.directory()?;
    only_entries(&parent, &[OsStr::new(BUNDLE)])?;
    if let Some(id) = observed(&stage.parent.path.join(BUNDLE))? {
        if id != stage.bundle {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Staged bundle ownership changed",
            ));
        }
        let bundle = directory(&stage.parent.path.join(BUNDLE), false)?;
        if identity(&bundle.metadata().map_err(io_error)?) != id {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Staged app changed",
            ));
        }
        remove_owned_contents(&bundle, id.device, control, &mut 100_000, 0)?;
        unchanged_directory(&stage.parent.path.join(BUNDLE), &bundle)?;
        rustix::fs::unlinkat(&parent, BUNDLE, AtFlags::REMOVEDIR).map_err(io_error)?;
    }
    unchanged_directory(&stage.root.path, &root)?;
    unchanged_directory(&stage.parent.path, &parent)?;
    rustix::fs::unlinkat(
        &root,
        stage.parent.path.file_name().unwrap(),
        AtFlags::REMOVEDIR,
    )
    .map_err(io_error)?;
    root.sync_all().map_err(io_error)
}

pub(super) async fn recover_receipt(
    receipt: Receipt,
    config: &Config,
    control: &Control,
) -> Result<Option<RetiredApp>, AppInstallError> {
    receipt.validate(config)?;
    let replayed = replay_lifecycle(&receipt, config, control).await;
    let cleaned = cleanup_work(&receipt, config, control).await;
    let replayed = replayed?;
    cleaned?;
    let data = receipt.data();
    let Some(stage) = data.stage.clone() else {
        receipt.remove()?;
        return Ok(None);
    };
    let root = stage.root.directory()?;
    let parent_id = observed(&stage.parent.path)?;
    if parent_id.is_some_and(|id| id != stage.parent.identity) {
        return Err(failure(
            AppInstallFailure::ConcurrentChange,
            "Staging directory changed during recovery",
        ));
    }
    if matches!(replayed, Replay::Aborted) {
        if parent_id.is_some() {
            cleanup_stage(&stage, control)?;
        }
        receipt.remove()?;
        return Ok(None);
    }
    let staged = observed(&stage.parent.path.join(BUNDLE))?;
    let retiring = matches!(data.lifecycle, Lifecycle::Retiring);
    if let Some(previous) = stage.transfer.as_ref().and_then(Transfer::previous) {
        if staged == Some(previous.identity) {
            let parent = stage.parent.directory()?;
            only_entries(&parent, &[OsStr::new(BUNDLE)])?;
            let bundle = directory(&stage.parent.path.join(BUNDLE), false)?;
            if identity(&bundle.metadata().map_err(io_error)?) != previous.identity {
                return Err(failure(
                    AppInstallFailure::ConcurrentChange,
                    "Retired bundle changed",
                ));
            }
            let current = installed_for(&receipt, config, false, control).await?;
            let current_handle = PinnedDirectory::new(directory(&current.path, false)?)?;
            if current_handle.identity != current.identity {
                return Err(failure(
                    AppInstallFailure::ConcurrentChange,
                    "Current app changed before retirement handoff",
                ));
            }
            let current_root = OwnedPath::capture(current.path.parent().unwrap().to_path_buf())?;
            current_root.directory()?;
            return Ok(Some(RetiredApp {
                bundle: stage.parent.path.join(BUNDLE),
                version: previous
                    .version
                    .as_ref()
                    .map(|version| Version::parse(version))
                    .transpose()
                    .map_err(io_error)?,
                ownership: RetiredOwnership {
                    current: OwnedPath {
                        path: current.path,
                        identity: current.identity,
                    },
                    current_root,
                    current_handle,
                    root: stage.root.path,
                    parent: stage.parent.path,
                    root_handle: PinnedDirectory::new(root)?,
                    parent_handle: PinnedDirectory::new(parent)?,
                    bundle_handle: PinnedDirectory::new(bundle)?,
                    receipt,
                },
            }));
        }
        if !retiring {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Retired bundle disappeared before handoff",
            ));
        }
    }
    if parent_id.is_some() {
        let parent = stage.parent.directory()?;
        only_entries(&parent, &[])?;
        rustix::fs::unlinkat(
            &root,
            stage.parent.path.file_name().unwrap(),
            AtFlags::REMOVEDIR,
        )
        .map_err(io_error)?;
        root.sync_all().map_err(io_error)?;
    }

    receipt.remove()?;
    Ok(None)
}

pub(super) async fn refresh_tokens(
    tokens: Vec<RetiredApp>,
    config: &Config,
    control: &Control,
) -> Result<Vec<RetiredApp>, AppInstallError> {
    let mut refreshed = Vec::new();
    for token in tokens {
        let receipt = token.ownership.receipt;
        let path = receipt.path();
        match recover_receipt(receipt, config, control).await {
            Ok(Some(token)) => refreshed.push(token),
            result => {
                let mut error = result.err().unwrap_or_else(|| {
                    failure(
                        AppInstallFailure::ConcurrentChange,
                        "Pending retirement disappeared",
                    )
                });
                error.residue.push(path);
                return Err(error);
            }
        }
    }
    Ok(refreshed)
}

pub(super) async fn recover(
    config: &Config,
    root: &File,
    control: &Control,
    attempt: &mut Attempt,
) -> Result<Vec<RetiredApp>, AppInstallError> {
    let mut receipts = Vec::new();
    let mut entries = Vec::new();
    for entry in fs::read_dir(&config.work).map_err(io_error)? {
        control.check()?;
        let entry = entry.map_err(io_error)?;
        if entries.len() >= 256 {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Too many pending installer objects",
            ));
        }
        entries.push(entry.file_name());
    }
    for name in &entries {
        if !name
            .to_str()
            .is_some_and(|name| name.starts_with("receipt-") && name.ends_with(".json"))
        {
            continue;
        }
        let result = (|| {
            let mut file = regular_at(root, name)?;
            let metadata = file.metadata().map_err(io_error)?;
            if metadata.len() > 32 * 1024 || metadata.mode() & 0o077 != 0 {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Unsafe transaction receipt",
                ));
            }
            let mut bytes = Vec::new();
            (&mut file)
                .take(32 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(io_error)?;
            if bytes.len() > 32 * 1024 {
                return Err(io_error("Receipt exceeds limit"));
            }
            let data = serde_json::from_slice(&bytes)
                .map_err(|error| failure(AppInstallFailure::UnsafePath, error))?;
            let receipt = Receipt {
                root: config.work.clone(),
                name: name.clone(),
                root_handle: PinnedDirectory::new(root.try_clone().map_err(io_error)?)?,
                state: Mutex::new((data, Some(file))),
            };
            receipt.validate(config)?;
            Ok(receipt)
        })()
        .map_err(|mut error: AppInstallError| {
            error.residue.push(config.work.join(name));
            error
        })?;
        receipts.push(result);
    }
    for name in entries {
        if name == ".koidrive-app-install.lock"
            || receipts.iter().any(|receipt| {
                receipt.name == name
                    || receipt.data().work.path.file_name() == Some(name.as_os_str())
            })
        {
            continue;
        }
        let mut error = failure(
            AppInstallFailure::UnsafePath,
            "Unowned installer residue requires inspection",
        );
        error.residue.push(config.work.join(name));
        return Err(error);
    }
    for path in [&config.system, &config.user] {
        if fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            continue;
        }
        let directory = directory(path, false)?;
        let mut count = 0;
        for entry in rustix::fs::Dir::read_from(&directory).map_err(io_error)? {
            control.check()?;
            count += 1;
            if count > 4096 {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Too many Applications entries to inspect safely",
                ));
            }
            let entry = entry.map_err(io_error)?;
            let name = entry.file_name();
            if !name.to_bytes().starts_with(b".koidrive-retired-") {
                continue;
            }
            let stage = path.join(OsStr::from_bytes(name.to_bytes()));
            if !receipts.iter().any(|receipt| {
                receipt
                    .data()
                    .stage
                    .as_ref()
                    .is_some_and(|owned| owned.parent.path == stage)
            }) {
                let mut error = failure(
                    AppInstallFailure::UnsafePath,
                    "Unowned app staging residue requires inspection",
                );
                error.residue.push(stage);
                return Err(error);
            }
        }
    }
    let mut retired = Vec::new();
    for receipt in receipts {
        let path = receipt.path();
        match recover_receipt(receipt, config, control).await {
            Ok(Some(token)) => {
                attempt.retain(&token);
                retired.push(token);
            }
            Ok(None) => (),
            Err(mut error) => {
                error.residue.push(path);
                return Err(error);
            }
        }
    }
    Ok(retired)
}
