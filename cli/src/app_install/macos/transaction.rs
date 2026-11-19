use crate::app_install::error::{AppInstallError, AppInstallFailure, failure};
use crate::app_install::facts::{AppLaunch, CommitChange, CommitFact};
use crate::app_install::macos::attempt::Attempt;
use crate::app_install::macos::config::Config;
use crate::app_install::macos::constants::{BUNDLE, CLEANUP, SIGNER};
use crate::app_install::macos::control::{
    CancelOnDrop, Control, Registry, io_error, passed, process,
};
use crate::app_install::macos::download::download;
use crate::app_install::macos::filesystem::{
    directory, identity, lock, observed, private_work, unchanged_directory, writable,
};
use crate::app_install::macos::lifecycle::{capture_ui, complete_lifecycle, open, quiesce};
use crate::app_install::macos::receipt::begin_receipt;
use crate::app_install::macos::recovery::{recover, recover_receipt, refresh_tokens};
use crate::app_install::macos::state::{
    Lifecycle, OwnedMount, OwnedPath, PreviousBundle, Receipt, Stage, Staged, Transfer,
    VerifiedBundle, WorkState,
};
use crate::app_install::macos::verification::{
    current, inventory, signature_args, unchanged_bundle, verification_error, verify,
};
use crate::app_install::outcome::{AppChange, AppInstall, AppLaunchResult, RetiredApp};
use rustix::fs::RenameFlags;
use semver::Version;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

#[cfg(test)]
pub(super) async fn ensure_with(
    config: Config,
    target: Version,
    launch: AppLaunch,
    deadline: Instant,
) -> Result<AppInstall, AppInstallError> {
    ensure_registered(config, target, launch, deadline, &Registry::default()).await
}
pub(in crate::app_install) async fn ensure_registered(
    config: Config,
    target: Version,
    launch: AppLaunch,
    deadline: Instant,
    registry: &Registry,
) -> Result<AppInstall, AppInstallError> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(300));
    if deadline <= Instant::now() {
        return Err(failure(
            AppInstallFailure::Deadline,
            "No time was left to install the app",
        ));
    }
    let (tx, rx) = watch::channel(false);
    let registration = registry.register(tx.clone());
    let guard = CancelOnDrop(tx);
    let task = tokio::spawn(async move {
        let _registration = registration;
        let control = Control {
            deadline,
            cancel: rx,
        };
        transaction(config, target, launch, control, deadline).await
    });
    let result = task
        .await
        .map_err(|error| failure(AppInstallFailure::Io, error))?;
    drop(guard);
    result
}
fn older(app: &VerifiedBundle, target: &Version) -> bool {
    app.version
        .as_ref()
        .is_none_or(|version| version.cmp_precedence(target).is_lt())
}
pub(super) async fn stage_copy(
    config: &Config,
    receipt: &Receipt,
    root: &Path,
    root_fd: &File,
    source: &Path,
    control: &Control,
) -> Result<Staged, AppInstallError> {
    let mut data = receipt.data();
    let version = Version::parse(&data.target).map_err(io_error)?;
    unchanged_directory(root, root_fd)?;
    let parent = tempfile::Builder::new()
        .prefix(".koidrive-retired-")
        .tempdir_in(root)
        .map_err(io_error)?
        .keep();
    let staged_path = parent.join(BUNDLE);
    fs::create_dir(&staged_path).map_err(io_error)?;
    let parent_fd = directory(&parent, false)?;
    let bundle = identity(
        &directory(&staged_path, false)?
            .metadata()
            .map_err(io_error)?,
    );
    data.stage = Some(Stage {
        root: OwnedPath {
            path: root.to_path_buf(),
            identity: identity(&root_fd.metadata().map_err(io_error)?),
        },
        parent: OwnedPath::capture(parent.clone())?,
        bundle,
        transfer: None,
    });
    parent_fd.sync_all().map_err(io_error)?;
    root_fd.sync_all().map_err(io_error)?;
    receipt.save(&data)?;
    process(
        &format!("Copying Koidrive into {}", root.display()),
        config.limits.tool,
        &config.ditto,
        &[passed(source)?, passed(&staged_path)?],
        control,
    )
    .await?;
    let staged = verify(config, &staged_path, Some(&version), control).await?;
    if staged.identity != bundle {
        return Err(failure(
            AppInstallFailure::ConcurrentChange,
            "Staged bundle was replaced during copy",
        ));
    }
    Ok(Staged {
        bundle: staged,
        parent,
        parent_fd,
    })
}
pub(super) async fn exchange(
    config: &Config,
    receipt: &Receipt,
    staged: Staged,
    installed: &[VerifiedBundle],
    requested: bool,
    control: &Control,
    attempt: &mut Attempt,
) -> Result<AppLaunchResult, AppInstallError> {
    let mut data = receipt.data();
    let version = Version::parse(&data.target).map_err(io_error)?;
    let stage = data
        .stage
        .as_mut()
        .ok_or_else(|| io_error("Missing app stage"))?;
    let root = stage.root.path.clone();
    let root_fd = stage.root.directory()?;
    let destination = root.join(BUNDLE);
    let previous = installed.iter().find(|app| app.path == destination);
    if previous.is_some() {
        process(
            "Checking the updater's signature",
            config.limits.tool,
            &config.codesign,
            &signature_args(&config.updater, SIGNER, false)?,
            control,
        )
        .await
        .map_err(|error| {
            verification_error(
                error,
                "Installer is not signed by the expected Developer ID",
            )
        })?;
    }
    stage.transfer = Some(Transfer {
        incoming: staged.bundle.identity,
        previous: previous.map(|app| PreviousBundle {
            identity: app.identity,
            version: app.version.as_ref().map(ToString::to_string),
        }),
    });
    let mut ui = capture_ui(config, installed, requested, control).await?;
    data.lifecycle = Lifecycle::Quiescing(ui.clone());
    receipt.save(&data)?;
    let mut paths: Vec<_> = installed.iter().map(|app| app.path.as_path()).collect();
    if !paths.contains(&destination.as_path()) {
        paths.push(&destination);
    }
    unchanged_directory(&root, &root_fd)?;
    for app in installed {
        if identity(&directory(&app.path, false)?.metadata().map_err(io_error)?) != app.identity {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "App changed before quiescing",
            ));
        }
    }
    ui.requested |= quiesce(config, &paths, control).await?;
    data.lifecycle = Lifecycle::Swapping(ui);
    receipt.save(&data)?;
    unchanged_directory(&root, &root_fd)?;
    unchanged_directory(&staged.parent, &staged.parent_fd)?;
    unchanged_directory(&receipt.root, &receipt.root_handle.file)?;
    control.check()?;
    if observed(&destination)? != previous.map(|app| app.identity)
        || observed(&staged.bundle.path)? != Some(staged.bundle.identity)
    {
        return Err(failure(
            AppInstallFailure::ConcurrentChange,
            "App changed before replacement",
        ));
    }
    rustix::fs::renameat_with(
        &staged.parent_fd,
        BUNDLE,
        &root_fd,
        BUNDLE,
        if previous.is_some() {
            RenameFlags::EXCHANGE
        } else {
            RenameFlags::NOREPLACE
        },
    )
    .map_err(io_error)?;
    attempt.committed.push(CommitFact {
        path: destination,
        version,
        change: if previous.is_some() {
            CommitChange::Replaced
        } else {
            CommitChange::Installed
        },
    });
    root_fd.sync_all().map_err(io_error)?;
    staged.parent_fd.sync_all().map_err(io_error)?;
    complete_lifecycle(receipt, config, control).await
}
async fn upgrade_alternatives(
    config: &Config,
    work_root: &File,
    target: &Version,
    chosen: &VerifiedBundle,
    control: &Control,
    cleanup_deadline: Instant,
    attempt: &mut Attempt,
) -> Result<(Vec<RetiredApp>, Vec<PathBuf>, bool), AppInstallError> {
    let installed = inventory(config, control).await?;
    let mut tokens = Vec::new();
    let mut preserved = Vec::new();
    let mut reopened = false;
    for app in installed
        .iter()
        .filter(|app| app.path != chosen.path && older(app, target))
    {
        let preserve = |mut error: AppInstallError| {
            error.residue.push(app.path.clone());
            error
        };
        let work = Control {
            deadline: cleanup_deadline
                .checked_sub(CLEANUP)
                .ok_or_else(|| {
                    failure(
                        AppInstallFailure::Deadline,
                        "No time remains for installer cleanup",
                    )
                })
                .map_err(preserve)?
                .min(control.deadline),
            cancel: control.cancel.clone(),
        };
        let root_path = app.path.parent().unwrap();
        let root = directory(root_path, false).map_err(preserve)?;
        if !writable(root_path).map_err(preserve)? {
            unchanged_directory(root_path, &root).map_err(preserve)?;
            unchanged_bundle(config, app, &work)
                .await
                .map_err(preserve)?;
            unchanged_directory(root_path, &root).map_err(preserve)?;
            preserved.push(app.path.clone());
            attempt.preserved.push(app.path.clone());
            continue;
        }
        let _root_lock = lock(&root, config.limits.lock, &work)
            .await
            .map_err(preserve)?;
        let receipt =
            begin_receipt(config, work_root, chosen.version.as_ref().unwrap()).map_err(preserve)?;
        attempt.receipts.push(receipt.path());
        let result = async {
            let staged =
                stage_copy(config, &receipt, root_path, &root, &chosen.path, &work).await?;
            if observed(&chosen.path)? != Some(chosen.identity) {
                return Err(failure(
                    AppInstallFailure::ConcurrentChange,
                    "Upgrade source changed during copy",
                ));
            }
            exchange(
                config,
                &receipt,
                staged,
                std::slice::from_ref(app),
                false,
                &work,
                attempt,
            )
            .await
        }
        .await;
        let receipt_path = receipt.path();
        let (_tx, cancel) = watch::channel(false);
        let cleanup = Control {
            deadline: cleanup_deadline.min(Instant::now() + CLEANUP),
            cancel,
        };
        let recovered = recover_receipt(receipt, config, &cleanup).await;
        if let Ok(Some(token)) = &recovered {
            attempt.retain(token);
        }
        match (result, recovered) {
            (Ok(launch), Ok(Some(token))) if !matches!(launch, AppLaunchResult::Failed(_)) => {
                reopened |= launch == AppLaunchResult::Requested;
                tokens.push(token);
            }
            (Err(mut error), _) | (_, Err(mut error)) => {
                error.residue.extend([app.path.clone(), receipt_path]);
                return Err(error);
            }
            _ => {
                let mut error = failure(
                    AppInstallFailure::Command,
                    "Alternative app upgrade is incomplete",
                );
                error.residue.extend([app.path.clone(), receipt_path]);
                return Err(error);
            }
        }
    }
    Ok((tokens, preserved, reopened))
}
async fn transaction(
    config: Config,
    target: Version,
    launch: AppLaunch,
    control: Control,
    cleanup_deadline: Instant,
) -> Result<AppInstall, AppInstallError> {
    let mut attempt = Attempt::default();
    match transaction_inner(
        config,
        target,
        launch,
        control,
        cleanup_deadline,
        &mut attempt,
    )
    .await
    {
        Ok(mut result) => {
            if let AppInstall::Present { committed, .. } = &mut result {
                *committed = attempt.committed;
            }
            Ok(result)
        }
        Err(error) => Err(attempt.fail(error)),
    }
}
async fn transaction_inner(
    config: Config,
    target: Version,
    launch: AppLaunch,
    mut control: Control,
    cleanup_deadline: Instant,
    attempt: &mut Attempt,
) -> Result<AppInstall, AppInstallError> {
    control.check()?;
    match fs::symlink_metadata(&config.work) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let installed = inventory(&config, &control).await?;
            if let Some(app) = current(&installed, &target)
                && !installed.iter().any(|other| older(other, &target))
            {
                unchanged_bundle(&config, app, &control).await?;
                let launch_result =
                    open(&config, &app.path, launch == AppLaunch::Open, &control).await;
                if launch == AppLaunch::Open {
                    unchanged_bundle(&config, app, &control).await?;
                }
                return Ok(AppInstall::Present {
                    change: AppChange::Unchanged,
                    committed: Vec::new(),
                    path: app.path.clone(),
                    version: app.version.clone().unwrap(),
                    launch: launch_result,
                    retired: Vec::new(),
                    residue: Vec::new(),
                    preserved: Vec::new(),
                });
            }
        }
        Err(error) => return Err(io_error(error)),
        Ok(_) => (),
    }
    let work_root = private_work(&config.work, true)?;
    let _user_lock = lock(&work_root, config.limits.lock, &control).await?;
    unchanged_directory(&config.work, &work_root)?;
    let mut retired = recover(&config, &work_root, &control, attempt).await?;
    let installed = inventory(&config, &control).await?;
    if let Some(app) = current(&installed, &target) {
        let (pending, preserved, reopened) = upgrade_alternatives(
            &config,
            &work_root,
            &target,
            app,
            &control,
            cleanup_deadline,
            attempt,
        )
        .await?;
        retired.extend(pending);
        unchanged_bundle(&config, app, &control).await?;
        let launch_result = if reopened {
            AppLaunchResult::Requested
        } else {
            open(&config, &app.path, launch == AppLaunch::Open, &control).await
        };
        let retired = refresh_tokens(retired, &config, &control).await?;
        unchanged_bundle(&config, app, &control).await?;
        return Ok(AppInstall::Present {
            change: AppChange::Unchanged,
            committed: Vec::new(),
            path: app.path.clone(),
            version: app.version.clone().unwrap(),
            launch: launch_result,
            retired,
            residue: Vec::new(),
            preserved,
        });
    }
    control.deadline = cleanup_deadline.checked_sub(CLEANUP).ok_or_else(|| {
        failure(
            AppInstallFailure::Deadline,
            "No time remains for installer cleanup",
        )
    })?;
    control.check()?;
    let system_root = directory(&config.system, false)?;
    let older_in = |root: &Path| {
        (installed.iter()).any(|app| app.path.parent() == Some(root) && older(app, &target))
    };
    let (root, root_fd) = if writable(&config.system)?
        && (older_in(&config.system) || !older_in(&config.user) || !writable(&config.user)?)
    {
        (config.system.clone(), system_root)
    } else {
        (config.user.clone(), directory(&config.user, true)?)
    };
    let _root_lock = lock(&root_fd, config.limits.lock, &control).await?;
    let receipt = begin_receipt(&config, &work_root, &target)?;
    attempt.receipts.push(receipt.path());
    let mut data = receipt.data();
    let work = data.work.path.clone();
    let result: Result<AppInstall, AppInstallError> = async {
        let dmg = download(&config, &target, &data.image, &control).await?;
        let mounted = OwnedMount {
            point: data.point.path.clone(),
            image: dmg.0,
        };
        data.work_state = WorkState::AttachPending;
        receipt.save(&data)?;
        mounted
            .attach(&config, data.image.identity, &control)
            .await?;
        let incoming = mounted.point.join(BUNDLE);
        verify(&config, &incoming, Some(&target), &control).await?;
        let staged = stage_copy(&config, &receipt, &root, &root_fd, &incoming, &control).await?;
        let installed = inventory(&config, &control).await?;
        if let Some(app) = current(&installed, &target) {
            unchanged_bundle(&config, app, &control).await?;
            return Ok(AppInstall::Present {
                change: AppChange::Unchanged,
                committed: Vec::new(),
                path: app.path.clone(),
                version: app.version.clone().unwrap(),
                launch: open(&config, &app.path, launch == AppLaunch::Open, &control).await,
                retired: Vec::new(),
                residue: Vec::new(),
                preserved: Vec::new(),
            });
        }
        let destination = root.join(BUNDLE);
        let previous = installed.iter().find(|app| app.path == destination);
        let launch_result = exchange(
            &config,
            &receipt,
            staged,
            &installed,
            launch == AppLaunch::Open,
            &control,
            attempt,
        )
        .await?;
        Ok(AppInstall::Present {
            committed: Vec::new(),
            change: if previous.is_some() {
                AppChange::Replaced
            } else {
                AppChange::Installed
            },
            path: destination,
            version: target.clone(),
            launch: launch_result,
            retired: Vec::new(),
            residue: Vec::new(),
            preserved: Vec::new(),
        })
    }
    .await;
    let (_tx, cancel) = watch::channel(false);
    let cleanup_control = Control {
        deadline: cleanup_deadline.min(Instant::now() + CLEANUP),
        cancel,
    };
    let receipt_path = receipt.path();
    let mut residue = Vec::new();
    match recover_receipt(receipt, &config, &cleanup_control).await {
        Ok(Some(token)) => {
            attempt.retain(&token);
            retired.push(token);
        }
        Ok(None) => (),
        Err(_) => residue.push(if work.exists() { work } else { receipt_path }),
    }
    match result {
        Ok(AppInstall::Present {
            change, mut launch, ..
        }) => {
            let mut cleanup_paths = residue.clone();
            cleanup_paths.extend(retired.iter().map(|token| token.bundle.clone()));
            let finished = async {
                let installed = inventory(&config, &cleanup_control).await?;
                let app = current(&installed, &target).ok_or_else(|| {
                    failure(
                        AppInstallFailure::Conflict,
                        "Installed app no longer satisfies target",
                    )
                })?;
                drop(_root_lock);
                let (pending, preserved, reopened) = upgrade_alternatives(
                    &config,
                    &work_root,
                    &target,
                    app,
                    &control,
                    cleanup_deadline,
                    attempt,
                )
                .await?;
                retired.extend(pending);
                let retired = refresh_tokens(retired, &config, &cleanup_control).await?;
                unchanged_bundle(&config, app, &cleanup_control).await?;
                if reopened && launch == AppLaunchResult::NotRequested {
                    launch = AppLaunchResult::Requested;
                }
                Ok(AppInstall::Present {
                    committed: Vec::new(),
                    change,
                    path: app.path.clone(),
                    version: app.version.clone().unwrap(),
                    launch,
                    retired,
                    residue,
                    preserved,
                })
            }
            .await;
            finished.map_err(|mut error: AppInstallError| {
                error.residue.extend(cleanup_paths);
                error
            })
        }
        Ok(other) => Ok(other),
        Err(mut error) => {
            error.residue.extend(residue);
            error
                .residue
                .extend(retired.iter().map(|token| token.bundle.clone()));
            Err(error)
        }
    }
}
