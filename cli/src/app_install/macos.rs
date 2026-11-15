mod control;
#[cfg(test)]
use control::ensure_with;
pub(super) use control::{active, ensure_registered};
use control::{args, io_error, process};
mod filesystem;
pub(super) use filesystem::discard_retired;
pub(crate) use filesystem::writable;
use filesystem::{directory, identity, lock, remove_owned_contents, unchanged_directory};
mod verification;
#[cfg(test)]
use verification::safe_tree;
pub(super) use verification::verify_helper;
use verification::{
    current, inventory, signature_args, unchanged_bundle, verification_error, verify,
};
mod download;
use download::download;
mod receipt;
use receipt::{begin_receipt, observed, only_entries, private_work, regular_at};
mod recovery;
use recovery::{recover, recover_receipt, refresh_tokens};
mod lifecycle;
use lifecycle::{capture_ui, complete_lifecycle, installed_for, open, quiesce, replay_lifecycle};
mod transaction;
use transaction::transaction;
#[cfg(test)]
use transaction::{exchange, stage_copy};
mod mount;
use super::*;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags, RenameFlags};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::watch;

const BUNDLE: &str = "Koidrive.app";
const IDENTIFIER: &str = "dev.koidrive.Koidrive";
const REQUIREMENT: &str = "anchor apple generic and identifier \"dev.koidrive.Koidrive\" and certificate leaf[subject.OU] = \"656SPP5XR8\" and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists";
const SIGNER: &str = "anchor apple generic and certificate leaf[subject.OU] = \"656SPP5XR8\" and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists";
const CLEANUP: Duration = Duration::from_secs(20);
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;

const INSPECT_RUNNING: &str = r#"ObjC.import('AppKit');
function run(args) {
    const inspectedBundlePath = ObjC.unwrap($(args[0]).stringByStandardizingPath);
    const apps = $.NSRunningApplication.runningApplicationsWithBundleIdentifier('dev.koidrive.Koidrive');
    for (let i = 0; i < apps.count; i++) {
        const app = apps.objectAtIndex(i);
        if (!app.terminated && app.bundleURL && ObjC.unwrap(app.bundleURL.path.stringByStandardizingPath) === inspectedBundlePath) return 'running';
    }
    return 'stopped';
}"#;

pub(crate) const QUIESCE: &str = r#"ObjC.import('AppKit');
function run(args) {
    const paths = args.map(p => ObjC.unwrap($(p).stringByStandardizingPath));
    const asked = [];
    let running = false;
    while (true) {
        const apps = $.NSRunningApplication.runningApplicationsWithBundleIdentifier('dev.koidrive.Koidrive');
        let remaining = false;
        for (let i = 0; i < apps.count; i++) {
            const app = apps.objectAtIndex(i);
            if (!app.bundleURL || paths.indexOf(ObjC.unwrap(app.bundleURL.path.stringByStandardizingPath)) < 0) continue;
            if (app.terminated) continue;
            remaining = true;
            running = true;
            if (asked.indexOf(app.processIdentifier) >= 0) continue;
            if (!app.terminate) throw Error('Koidrive refused graceful termination');
            asked.push(app.processIdentifier);
        }
        if (!remaining) return running ? 'running' : 'stopped';
        $.NSThread.sleepForTimeInterval(0.05);
    }
}"#;

#[derive(Clone)]
pub(super) struct Config {
    system: PathBuf,
    user: PathBuf,
    work: PathBuf,
    release: String,
    codesign: PathBuf,
    spctl: PathBuf,
    hdiutil: PathBuf,
    ditto: PathBuf,
    osascript: PathBuf,
    open: PathBuf,
    updater: PathBuf,
    max_download: u64,
}

impl Config {
    pub(super) fn production() -> Result<Self, AppInstallError> {
        let home = PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| failure(AppInstallFailure::UnsafePath, "HOME is not set"))?,
        );
        if !home.is_absolute() {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "HOME is not absolute",
            ));
        }
        Ok(Self {
            system: PathBuf::from("/Applications"),
            user: home.join("Applications"),
            work: home.join("Library/Caches/dev.koidrive.app-install"),
            release: "https://github.com/koidrive/koidrive/releases/download".into(),
            codesign: "/usr/bin/codesign".into(),
            spctl: "/usr/sbin/spctl".into(),
            hdiutil: "/usr/bin/hdiutil".into(),
            ditto: "/usr/bin/ditto".into(),
            osascript: "/usr/bin/osascript".into(),
            open: "/usr/bin/open".into(),
            updater: std::env::current_exe().map_err(io_error)?,
            max_download: 1024 * 1024 * 1024,
        })
    }
}

struct CancelOnDrop(watch::Sender<bool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

struct Active {
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
}

#[derive(Clone, Default)]
pub(super) struct Registry(Arc<Mutex<Vec<Arc<Active>>>>);

impl Registry {
    fn register(&self, cancel: watch::Sender<bool>) -> Registration {
        let (finished, rx) = watch::channel(false);
        let active = Arc::new(Active {
            cancel,
            finished: rx,
        });
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(active.clone());
        Registration {
            registry: self.clone(),
            active,
            finished,
        }
    }

    pub(super) async fn cancel(&self) {
        let transactions = {
            let active = self.0.lock().unwrap_or_else(|error| error.into_inner());
            for transaction in active.iter() {
                transaction.cancel.send_replace(true);
            }
            active.clone()
        };
        for transaction in transactions {
            let mut finished = transaction.finished.clone();
            while !*finished.borrow_and_update() {
                if finished.changed().await.is_err() {
                    break;
                }
            }
        }
    }
}

struct Registration {
    registry: Registry,
    active: Arc<Active>,
    finished: watch::Sender<bool>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|entry| !Arc::ptr_eq(entry, &self.active));
        self.finished.send_replace(true);
    }
}

#[derive(Clone)]
struct Control {
    deadline: Instant,
    cancel: watch::Receiver<bool>,
}

impl Control {
    fn check(&self) -> Result<(), AppInstallError> {
        if *self.cancel.borrow() {
            return Err(failure(
                AppInstallFailure::Cancelled,
                "App installation cancelled",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(failure(
                AppInstallFailure::Deadline,
                "App installation deadline exceeded",
            ));
        }
        Ok(())
    }

    async fn cancelled(&self) {
        let mut rx = self.cancel.clone();
        if !*rx.borrow() {
            let _ = rx.changed().await;
        }
    }

    async fn bounded<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, AppInstallError>>,
    ) -> Result<T, AppInstallError> {
        self.check()?;
        tokio::select! {
            biased;
            _ = self.cancelled() => Err(failure(AppInstallFailure::Cancelled, "App installation cancelled")),
            _ = tokio::time::sleep_until(self.deadline) => Err(failure(AppInstallFailure::Deadline, "App installation deadline exceeded")),
            result = future => result,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    device: u64,
    inode: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct RetiredOwnership {
    root: PathBuf,
    parent: PathBuf,
    root_handle: PinnedDirectory,
    parent_handle: PinnedDirectory,
    bundle_handle: PinnedDirectory,
    current: OwnedPath,
    current_root: OwnedPath,
    current_handle: PinnedDirectory,
    receipt: Receipt,
}

#[derive(Debug)]
struct PinnedDirectory {
    file: File,
    identity: Identity,
}

impl PinnedDirectory {
    fn new(file: File) -> Result<Self, AppInstallError> {
        let identity = identity(&file.metadata().map_err(io_error)?);
        Ok(Self { file, identity })
    }
}

impl PartialEq for PinnedDirectory {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}

impl Eq for PinnedDirectory {}

struct InstallerLock(File);

impl Drop for InstallerLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, FlockOperation::Unlock);
    }
}

#[derive(Clone)]
struct VerifiedBundle {
    path: PathBuf,
    version: Option<Version>,
    identity: Identity,
}

pub(crate) struct VerifiedHelper {
    config: Config,
    app: VerifiedBundle,
    binary: PathBuf,
    root: PinnedDirectory,
    bundle: PinnedDirectory,
    helper_file: File,
}

impl VerifiedHelper {
    pub(crate) fn path(&self) -> &Path {
        &self.binary
    }

    pub(crate) fn version(&self) -> Option<&Version> {
        self.app.version.as_ref()
    }

    pub(crate) fn require_version(&self, expected: &str) -> Result<(), AppInstallError> {
        if self.version().map(ToString::to_string).as_deref() != Some(expected) {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "App version changed since installation verification",
            ));
        }
        Ok(())
    }

    pub(crate) async fn check(&self, deadline: Instant) -> Result<(), AppInstallError> {
        let (_tx, cancel) = watch::channel(false);
        let control = Control { deadline, cancel };
        unchanged_directory(self.app.path.parent().unwrap(), &self.root.file)?;
        unchanged_directory(&self.app.path, &self.bundle.file)?;
        unchanged_bundle(&self.config, &self.app, &control).await?;
        let helper = File::from(
            rustix::fs::open(
                &self.binary,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io_error)?,
        );
        if identity(&helper.metadata().map_err(io_error)?)
            != identity(&self.helper_file.metadata().map_err(io_error)?)
        {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "App helper changed since verification",
            ));
        }
        Ok(())
    }
}

struct VerifiedDmg(PathBuf);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedPath {
    path: PathBuf,
    identity: Identity,
}

impl OwnedPath {
    fn capture(path: PathBuf) -> Result<Self, AppInstallError> {
        let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
        if metadata.file_type().is_symlink() {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Symlinked transaction object",
            ));
        }
        Ok(Self {
            path,
            identity: identity(&metadata),
        })
    }

    fn directory(&self) -> Result<File, AppInstallError> {
        let file = directory(&self.path, false)?;
        if identity(&file.metadata().map_err(io_error)?) != self.identity {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Transaction directory changed",
            ));
        }
        Ok(file)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousBundle {
    identity: Identity,
    version: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Transfer {
    incoming: Identity,
    previous: Option<PreviousBundle>,
}

impl Transfer {
    fn previous(&self) -> Option<&PreviousBundle> {
        self.previous.as_ref()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RunningApp {
    bundle: OwnedPath,
    root: OwnedPath,
    version: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UiIntent {
    requested: bool,
    running: Vec<RunningApp>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
enum Lifecycle {
    Preparing,
    Quiescing(UiIntent),
    Swapping(UiIntent),
    Installed(UiIntent),
    Reopening(UiIntent),
    AwaitingHandoff,
    Retiring,
    Aborting(UiIntent),
}

impl Lifecycle {
    fn ui(&self) -> UiIntent {
        match self {
            Self::Quiescing(ui)
            | Self::Swapping(ui)
            | Self::Installed(ui)
            | Self::Reopening(ui)
            | Self::Aborting(ui) => ui.clone(),
            _ => UiIntent::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
enum WorkState {
    Downloading,
    AttachPending,
    Cleaning { attached: bool },
    Cleaned,
}

impl WorkState {
    fn attached(self) -> bool {
        matches!(
            self,
            Self::AttachPending | Self::Cleaning { attached: true }
        )
    }
    fn cleaning(self) -> bool {
        matches!(self, Self::Cleaning { .. } | Self::Cleaned)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Stage {
    root: OwnedPath,
    parent: OwnedPath,
    bundle: Identity,
    transfer: Option<Transfer>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptData {
    schema: u8,
    work_root: Identity,
    work: OwnedPath,
    image: OwnedPath,
    point: OwnedPath,
    target: String,
    work_state: WorkState,
    lifecycle: Lifecycle,
    stage: Option<Stage>,
}

#[derive(Debug)]
struct Receipt {
    root: PathBuf,
    name: OsString,
    root_handle: PinnedDirectory,
    state: Mutex<(ReceiptData, Option<File>)>,
}

impl PartialEq for Receipt {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root && self.name == other.name && self.root_handle == other.root_handle
    }
}

impl Eq for Receipt {}

enum Replay {
    Empty,
    Aborted,
    Committed,
}

struct OwnedMount {
    point: PathBuf,
    image: PathBuf,
}

enum MountState {
    Absent,
    Owned,
    Foreign,
}

struct Staged {
    bundle: VerifiedBundle,
    parent: PathBuf,
    parent_fd: File,
}

#[derive(Default)]
struct Attempt {
    committed: Vec<CommitFact>,
    retired: Vec<PathBuf>,
    preserved: Vec<PathBuf>,
    receipts: Vec<PathBuf>,
}

impl Attempt {
    fn retain(&mut self, token: &RetiredApp) {
        self.retired.push(token.bundle.clone());
        self.receipts.push(token.ownership.receipt.path());
    }

    fn fail(self, mut error: AppInstallError) -> AppInstallError {
        error.committed.extend(self.committed);
        error.retired.extend(self.retired);
        error.preserved.extend(self.preserved);
        error.residue.extend(
            self.receipts
                .into_iter()
                .filter(|path| fs::symlink_metadata(path).is_ok()),
        );
        for paths in [&mut error.residue, &mut error.retired, &mut error.preserved] {
            paths.sort();
            paths.dedup();
        }
        error
    }
}

#[cfg(test)]
pub(super) mod tests;
