use crate::app_install::error::{AppInstallError, AppInstallFailure, failure};
use crate::app_install::macos::constants::{BUNDLE, CLEANUP};
use crate::app_install::macos::control::{Control, io_error};
use crate::app_install::macos::filesystem::{
    InstallerLock, regular_at, remove_owned_contents, unchanged_directory,
};
use crate::app_install::macos::state::{Lifecycle, Transfer};
use crate::app_install::outcome::RetiredApp;
use rustix::fs::{AtFlags, FlockOperation};
use std::ffi::OsStr;
use tokio::sync::watch;
use tokio::time::Instant;

impl RetiredApp {
    pub(crate) fn discard(&self) -> Result<(), AppInstallError> {
        discard_retired(self)
    }
}
pub(in crate::app_install) fn discard_retired(retired: &RetiredApp) -> Result<(), AppInstallError> {
    let result = discard_owned(retired);
    result.map_err(|mut error| {
        error.residue.push(retired.ownership.parent.join(BUNDLE));
        error
    })
}
fn discard_owned(retired: &RetiredApp) -> Result<(), AppInstallError> {
    let ownership = &retired.ownership;
    ownership.current_root.directory()?;
    unchanged_directory(&ownership.current.path, &ownership.current_handle.file)?;
    let receipt_root = &ownership.receipt.root_handle.file;
    unchanged_directory(&ownership.receipt.root, receipt_root)?;
    let user_lock = regular_at(receipt_root, OsStr::new(".koidrive-app-install.lock"))?;
    rustix::fs::flock(&user_lock, FlockOperation::NonBlockingLockExclusive)
        .map_err(|error| failure(AppInstallFailure::ConcurrentChange, error))?;
    let _user_lock = InstallerLock(user_lock);
    let mut data = ownership.receipt.data();
    if !matches!(
        data.lifecycle,
        Lifecycle::AwaitingHandoff | Lifecycle::Retiring
    ) {
        return Err(failure(
            AppInstallFailure::Conflict,
            "App lifecycle is not ready for retirement",
        ));
    }
    let stage = data
        .stage
        .as_ref()
        .ok_or_else(|| io_error("Missing retirement stage"))?;
    if stage.root.path != ownership.root
        || stage.parent.path != ownership.parent
        || stage
            .transfer
            .as_ref()
            .and_then(Transfer::previous)
            .map(|previous| previous.identity)
            != Some(ownership.bundle_handle.identity)
    {
        return Err(failure(
            AppInstallFailure::UnsafePath,
            "Retirement receipt does not match token",
        ));
    }
    if retired.bundle != ownership.parent.join(BUNDLE)
        || ownership.parent.parent() != Some(ownership.root.as_path())
    {
        return Err(failure(
            AppInstallFailure::UnsafePath,
            "Retired app path does not match its ownership",
        ));
    }
    let root = &ownership.root_handle.file;
    let parent = &ownership.parent_handle.file;
    let bundle = &ownership.bundle_handle.file;
    unchanged_directory(&ownership.root, root)?;
    unchanged_directory(&ownership.parent, parent)?;
    unchanged_directory(&retired.bundle, bundle)?;
    let entries = rustix::fs::Dir::read_from(parent).map_err(io_error)?;
    for entry in entries {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." && name != BUNDLE.as_bytes() {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Retired app parent contains an unowned entry",
            ));
        }
    }
    let (_tx, cancel) = watch::channel(false);
    let control = Control {
        deadline: Instant::now() + CLEANUP,
        cancel,
    };
    ownership.current_root.directory()?;
    unchanged_directory(&ownership.current.path, &ownership.current_handle.file)?;
    data.lifecycle = Lifecycle::Retiring;
    ownership.receipt.save(&data)?;
    unchanged_directory(&ownership.root, root)?;
    unchanged_directory(&ownership.parent, parent)?;
    unchanged_directory(&retired.bundle, bundle)?;
    let mut remaining = 100_000;
    remove_owned_contents(
        bundle,
        ownership.bundle_handle.identity.device,
        &control,
        &mut remaining,
        0,
    )?;
    control.check()?;
    unchanged_directory(&ownership.root, root)?;
    unchanged_directory(&ownership.parent, parent)?;
    unchanged_directory(&retired.bundle, bundle)?;
    rustix::fs::unlinkat(parent, BUNDLE, AtFlags::REMOVEDIR).map_err(io_error)?;
    let name = ownership.parent.file_name().ok_or_else(|| {
        failure(
            AppInstallFailure::UnsafePath,
            "Retired app parent has no name",
        )
    })?;
    unchanged_directory(&ownership.parent, parent)?;
    rustix::fs::unlinkat(root, name, AtFlags::REMOVEDIR).map_err(io_error)?;
    root.sync_all().map_err(io_error)?;
    ownership.receipt.remove()
}
