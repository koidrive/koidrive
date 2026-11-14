// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

#[cfg(target_os = "macos")]
pub(crate) use macos::VerifiedHelper;
#[cfg(all(test, target_os = "macos"))]
pub(crate) use macos::tests::pending_retirements_fixture;
#[cfg(all(test, target_os = "macos"))]
pub(crate) use macos::tests::verified_helper_fixture;
#[cfg(all(test, target_os = "macos"))]
pub(crate) use macos::tests::verified_helper_fixture_with_metadata;

use semver::Version;
use tokio::time::Instant;

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
pub(crate) struct CommitFact {
    pub(crate) path: PathBuf,
    #[serde(
        serialize_with = "serialize_version",
        deserialize_with = "deserialize_version"
    )]
    #[schemars(with = "String")]
    pub(crate) version: Version,
    pub(crate) change: CommitChange,
}

fn serialize_version<S: serde::Serializer>(
    version: &Version,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(version)
}

fn deserialize_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Version, D::Error> {
    let text = <String as serde::Deserialize>::deserialize(deserializer)?;
    Version::parse(&text).map_err(serde::de::Error::custom)
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommitChange {
    Installed,
    Replaced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppLaunch {
    Open,
    Preserve,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AppInstall {
    Skipped(AppSkip),
    #[cfg(target_os = "macos")]
    Present {
        change: AppChange,
        committed: Vec<CommitFact>,
        path: PathBuf,
        version: Version,
        launch: AppLaunchResult,
        retired: Vec<RetiredApp>,
        residue: Vec<PathBuf>,
        preserved: Vec<PathBuf>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AppSkip {
    Ci,
    Platform,
    Architecture,
}

#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AppChange {
    Unchanged,
    Installed,
    Replaced,
}

#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AppLaunchResult {
    NotRequested,
    Requested,
    Failed(String),
}

#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RetiredApp {
    pub(crate) bundle: PathBuf,
    pub(crate) version: Option<Version>,
    ownership: macos::RetiredOwnership,
}

#[cfg(target_os = "macos")]
impl RetiredApp {
    pub(crate) fn discard(&self) -> Result<(), AppInstallError> {
        macos::discard_retired(self)
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) type AppInstallError = std::convert::Infallible;

#[cfg(target_os = "macos")]
pub(crate) type AppInstallError = Box<AppInstallErrorDetails>;

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(crate) struct AppInstallErrorDetails {
    pub(crate) kind: AppInstallFailure,
    pub(crate) message: String,
    pub(crate) residue: Vec<PathBuf>,
    pub(crate) committed: Vec<CommitFact>,
    pub(crate) retired: Vec<PathBuf>,
    pub(crate) preserved: Vec<PathBuf>,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppInstallFailure {
    Deadline,
    Cancelled,
    Download,
    Verification,
    Conflict,
    UnsafePath,
    ConcurrentChange,
    InUse,
    Command,
    Io,
}

#[cfg(target_os = "macos")]
impl std::fmt::Display for AppInstallErrorDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[cfg(target_os = "macos")]
impl std::error::Error for AppInstallErrorDetails {}

#[cfg(target_os = "macos")]
fn failure(kind: AppInstallFailure, message: impl ToString) -> AppInstallError {
    Box::new(AppInstallErrorDetails {
        kind,
        message: message.to_string(),
        residue: Vec::new(),
        committed: Vec::new(),
        retired: Vec::new(),
        preserved: Vec::new(),
    })
}

fn skip_reason(ci: bool, macos: bool, arm64: bool) -> Option<AppSkip> {
    if ci {
        Some(AppSkip::Ci)
    } else if !macos {
        Some(AppSkip::Platform)
    } else if !arm64 {
        Some(AppSkip::Architecture)
    } else {
        None
    }
}

pub(crate) async fn ensure(
    target: &Version,
    launch: AppLaunch,
    deadline: Instant,
) -> Result<AppInstall, AppInstallError> {
    if let Some(reason) = skip_reason(
        std::env::var_os("CI").is_some(),
        cfg!(target_os = "macos"),
        cfg!(target_arch = "aarch64"),
    ) {
        return Ok(AppInstall::Skipped(reason));
    }
    #[cfg(target_os = "macos")]
    {
        macos::ensure_registered(
            macos::Config::production()?,
            target.clone(),
            launch,
            deadline,
            macos::active(),
        )
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (target, launch, deadline);
        Ok(AppInstall::Skipped(AppSkip::Platform))
    }
}

pub(crate) async fn cancel_active() {
    #[cfg(target_os = "macos")]
    macos::active().cancel().await;
}

#[cfg(target_os = "macos")]
pub(crate) async fn verify_existing(
    helper: &std::path::Path,
    deadline: Instant,
) -> Result<VerifiedHelper, AppInstallError> {
    macos::verify_helper(macos::Config::production()?, helper, deadline).await
}

#[cfg(target_os = "macos")]
pub(crate) use macos::QUIESCE;

#[cfg(target_os = "macos")]
mod macos {
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
            if (!app.terminate()) throw Error('Koidrive refused graceful termination');
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

    pub(super) fn active() -> &'static Registry {
        static ACTIVE: OnceLock<Registry> = OnceLock::new();
        ACTIVE.get_or_init(Registry::default)
    }

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

    #[cfg(test)]
    async fn ensure_with(
        config: Config,
        target: Version,
        launch: AppLaunch,
        deadline: Instant,
    ) -> Result<AppInstall, AppInstallError> {
        ensure_registered(config, target, launch, deadline, &Registry::default()).await
    }

    pub(super) async fn ensure_registered(
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
                "App installation deadline exceeded",
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

    fn io_error(error: impl ToString) -> AppInstallError {
        failure(AppInstallFailure::Io, error)
    }

    async fn process(
        program: &Path,
        args: &[OsString],
        control: &Control,
    ) -> Result<Vec<u8>, AppInstallError> {
        control.check()?;
        let mut child = Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(io_error)?;
        let process_group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw);
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io_error("Missing child stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io_error("Missing child stderr"))?;
        let read = async {
            async fn read_output(
                reader: &mut (impl tokio::io::AsyncRead + Unpin),
            ) -> Result<Vec<u8>, AppInstallError> {
                let mut bytes = Vec::new();
                reader
                    .take(MAX_OUTPUT + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(io_error)?;
                if bytes.len() as u64 > MAX_OUTPUT {
                    return Err(failure(
                        AppInstallFailure::Command,
                        "Installer tool output exceeds limit",
                    ));
                }
                Ok(bytes)
            }
            let (status, out, err) = tokio::try_join!(
                async { child.wait().await.map_err(io_error) },
                read_output(&mut stdout),
                read_output(&mut stderr),
            )?;
            if !status.success() {
                return Err(failure(
                    AppInstallFailure::Command,
                    format!(
                        "{} failed ({status}): {}",
                        program.display(),
                        String::from_utf8_lossy(&err)
                    ),
                ));
            }
            let mut output = out;
            if !err.is_empty() {
                output.push(b'\n');
                output.extend(err);
            }
            Ok(output)
        };
        let result = control.bounded(read).await;
        if result.is_err() {
            if let Some(pid) = process_group {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        result
    }

    fn args(parts: &[&OsStr]) -> Vec<OsString> {
        parts.iter().map(|part| part.to_os_string()).collect()
    }

    fn signature_args(path: &Path, requirement: &str, deep: bool) -> Vec<OsString> {
        let mut arguments = args(&[OsStr::new("--verify")]);
        if deep {
            arguments.push("--deep".into());
        }
        arguments.extend(args(&[
            OsStr::new("--strict"),
            OsStr::new("-R"),
            OsStr::new(&format!("={requirement}")),
            path.as_os_str(),
        ]));
        arguments
    }

    fn directory(path: &Path, create: bool) -> Result<File, AppInstallError> {
        let mut fd = rustix::fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io_error)?;
        for component in path.components() {
            match component {
                Component::RootDir => continue,
                Component::Normal(name) => {
                    if create {
                        match rustix::fs::mkdirat(&fd, name, Mode::from_raw_mode(0o700)) {
                            Ok(()) | Err(rustix::io::Errno::EXIST) => (),
                            Err(error) => return Err(io_error(error)),
                        }
                    }
                    fd = rustix::fs::openat(
                        &fd,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|error| {
                        failure(
                            AppInstallFailure::UnsafePath,
                            format!("Unsafe directory {}: {error}", path.display()),
                        )
                    })?;
                }
                _ => {
                    return Err(failure(
                        AppInstallFailure::UnsafePath,
                        "Installer paths must be absolute and normalized",
                    ));
                }
            }
        }
        if !path.is_absolute() {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Installer directory is not absolute",
            ));
        }
        Ok(fd.into())
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

    fn identity(metadata: &fs::Metadata) -> Identity {
        Identity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    pub(super) fn discard_retired(retired: &RetiredApp) -> Result<(), AppInstallError> {
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

    fn remove_owned_contents(
        directory: &File,
        device: u64,
        control: &Control,
        remaining: &mut usize,
        depth: usize,
    ) -> Result<(), AppInstallError> {
        if depth > 128 {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Retired app directory nesting exceeds limit",
            ));
        }
        for entry in rustix::fs::Dir::read_from(directory).map_err(io_error)? {
            control.check()?;
            let entry = entry.map_err(io_error)?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            *remaining = remaining.checked_sub(1).ok_or_else(|| {
                failure(
                    AppInstallFailure::UnsafePath,
                    "Retired app entry count exceeds limit",
                )
            })?;
            let stat =
                rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io_error)?;
            if stat.st_dev as u64 != device {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Retired app contains a mounted filesystem",
                ));
            }
            let flags = match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
                rustix::fs::FileType::Directory => {
                    let child = File::from(
                        rustix::fs::openat(
                            directory,
                            name,
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )
                        .map_err(io_error)?,
                    );
                    let metadata = child.metadata().map_err(io_error)?;
                    if metadata.dev() != device || metadata.ino() != stat.st_ino {
                        return Err(failure(
                            AppInstallFailure::ConcurrentChange,
                            "Retired app child changed",
                        ));
                    }
                    remove_owned_contents(&child, device, control, remaining, depth + 1)?;
                    AtFlags::REMOVEDIR
                }
                rustix::fs::FileType::RegularFile => AtFlags::empty(),
                rustix::fs::FileType::Symlink => {
                    let target =
                        rustix::fs::readlinkat(directory, name, Vec::new()).map_err(io_error)?;
                    let mut level = depth;
                    for component in Path::new(OsStr::from_bytes(target.as_bytes())).components() {
                        match component {
                            Component::CurDir => (),
                            Component::Normal(_) => level += 1,
                            Component::ParentDir if level > 0 => level -= 1,
                            _ => {
                                return Err(failure(
                                    AppInstallFailure::UnsafePath,
                                    "Retired app contains an escaping symlink",
                                ));
                            }
                        }
                    }
                    AtFlags::empty()
                }
                _ => {
                    return Err(failure(
                        AppInstallFailure::UnsafePath,
                        "Retired app contains a special file",
                    ));
                }
            };
            let current =
                rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io_error)?;
            if current.st_dev != stat.st_dev || current.st_ino != stat.st_ino {
                return Err(failure(
                    AppInstallFailure::ConcurrentChange,
                    "Retired app entry changed before removal",
                ));
            }
            rustix::fs::unlinkat(directory, name, flags).map_err(io_error)?;
        }
        Ok(())
    }

    fn unchanged_directory(path: &Path, fd: &File) -> Result<(), AppInstallError> {
        let current = directory(path, false)?;
        if identity(&current.metadata().map_err(io_error)?)
            != identity(&fd.metadata().map_err(io_error)?)
        {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Applications directory changed during installation",
            ));
        }
        Ok(())
    }

    struct InstallerLock(File);

    impl Drop for InstallerLock {
        fn drop(&mut self) {
            let _ = rustix::fs::flock(&self.0, FlockOperation::Unlock);
        }
    }

    async fn lock(root: &File, control: &Control) -> Result<InstallerLock, AppInstallError> {
        let name = ".koidrive-app-install.lock";
        let fd = match rustix::fs::openat(
            root,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::EXIST) => rustix::fs::openat(
                root,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io_error)?,
            Err(error) => return Err(io_error(error)),
        };
        let file = File::from(fd);
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Unsafe installer lock",
            ));
        }
        loop {
            control.check()?;
            match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    control
                        .bounded(async {
                            tokio::time::sleep(Duration::from_millis(40)).await;
                            Ok(())
                        })
                        .await?
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        let guard = InstallerLock(file);
        let current =
            rustix::fs::statat(root, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io_error)?;
        if current.st_ino != metadata.ino() || current.st_dev as u64 != metadata.dev() {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Installer lock changed",
            ));
        }
        Ok(guard)
    }

    fn safe_tree(bundle: &Path, control: &Control) -> Result<(), AppInstallError> {
        let metadata = fs::symlink_metadata(bundle).map_err(io_error)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "App bundle is not a real directory",
            ));
        }
        let canonical = fs::canonicalize(bundle).map_err(io_error)?;
        let mut pending = vec![bundle.to_path_buf()];
        let mut count = 0u64;
        let mut bytes = 0u64;
        while let Some(path) = pending.pop() {
            control.check()?;
            count += 1;
            if count > 100_000 {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "App has too many entries",
                ));
            }
            let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
            if metadata.file_type().is_symlink() {
                let resolved = fs::canonicalize(&path).map_err(io_error)?;
                if !resolved.starts_with(&canonical) {
                    return Err(failure(
                        AppInstallFailure::UnsafePath,
                        "App contains an escaping symlink",
                    ));
                }
            } else if metadata.is_dir() {
                for entry in fs::read_dir(&path).map_err(io_error)? {
                    pending.push(entry.map_err(io_error)?.path());
                }
            } else if metadata.is_file() {
                bytes = bytes.saturating_add(metadata.len());
                if bytes > 2 * 1024 * 1024 * 1024 {
                    return Err(failure(
                        AppInstallFailure::UnsafePath,
                        "App exceeds expanded size limit",
                    ));
                }
            } else {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "App contains a special file",
                ));
            }
        }
        Ok(())
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

    pub(super) async fn verify_helper(
        config: Config,
        helper: &Path,
        deadline: Instant,
    ) -> Result<VerifiedHelper, AppInstallError> {
        if !helper.is_absolute() || !crate::app::bundled(helper) {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Not an absolute app helper path",
            ));
        }
        let path = helper.ancestors().nth(3).unwrap();
        let root = PinnedDirectory::new(directory(path.parent().unwrap(), false)?)?;
        let bundle = PinnedDirectory::new(directory(path, false)?)?;
        let helper_file = File::from(
            rustix::fs::open(
                helper,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io_error)?,
        );
        let metadata = helper_file.metadata().map_err(io_error)?;
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "App helper is not an executable regular file",
            ));
        }
        let (_tx, cancel) = watch::channel(false);
        let control = Control { deadline, cancel };
        let app = verify(&config, path, None, &control).await?;
        let verified = VerifiedHelper {
            config,
            app,
            binary: helper.to_path_buf(),
            root,
            bundle,
            helper_file,
        };
        verified.check(deadline).await?;
        Ok(verified)
    }

    async fn verify(
        config: &Config,
        path: &Path,
        target: Option<&Version>,
        control: &Control,
    ) -> Result<VerifiedBundle, AppInstallError> {
        safe_tree(path, control)?;
        let id = identity(&fs::symlink_metadata(path).map_err(io_error)?);
        process(
            &config.codesign,
            &signature_args(path, REQUIREMENT, true),
            control,
        )
        .await
        .map_err(|error| verification_error(error, "App signature verification failed"))?;
        if target.is_some() {
            let assessment = process(
                &config.spctl,
                &args(&[
                    OsStr::new("--assess"),
                    OsStr::new("--type"),
                    OsStr::new("execute"),
                    OsStr::new("--verbose=2"),
                    path.as_os_str(),
                ]),
                control,
            )
            .await
            .map_err(|error| verification_error(error, "App notarization assessment failed"))?;
            let assessment = String::from_utf8_lossy(&assessment);
            let sources: Vec<_> = assessment
                .lines()
                .filter_map(|line| line.strip_prefix("source="))
                .collect();
            if sources != ["Notarized Developer ID"] {
                return Err(failure(
                    AppInstallFailure::Verification,
                    "App is not a Notarized Developer ID app",
                ));
            }
        }
        let plist_path = path.join("Contents/Info.plist");
        if fs::metadata(&plist_path).map_err(io_error)?.len() > 1024 * 1024 {
            return Err(failure(
                AppInstallFailure::Verification,
                "App Info.plist exceeds limit",
            ));
        }
        let plist = plist::Value::from_file(plist_path)
            .map_err(|error| failure(AppInstallFailure::Verification, error))?;
        let dict = plist.as_dictionary().ok_or_else(|| {
            failure(
                AppInstallFailure::Verification,
                "App Info.plist is not a dictionary",
            )
        })?;
        if dict
            .get("CFBundleIdentifier")
            .and_then(plist::Value::as_string)
            != Some(IDENTIFIER)
        {
            return Err(failure(
                AppInstallFailure::Verification,
                "Wrong app bundle identifier",
            ));
        }
        let version_kind = if target.is_some() {
            AppInstallFailure::Verification
        } else {
            AppInstallFailure::Conflict
        };
        let version = match dict.get("KoidriveVersion") {
            None if target.is_none() => None,
            value => {
                let text = value.and_then(plist::Value::as_string).ok_or_else(|| {
                    failure(version_kind, "App has no valid full KoidriveVersion")
                })?;
                Some(Version::parse(text).map_err(|error| failure(version_kind, error))?)
            }
        };
        if target.is_some_and(|target| version.as_ref() != Some(target)) {
            return Err(failure(
                AppInstallFailure::Verification,
                "App full release version does not match target",
            ));
        }
        if identity(&fs::symlink_metadata(path).map_err(io_error)?) != id {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "App changed during verification",
            ));
        }
        Ok(VerifiedBundle {
            path: path.to_path_buf(),
            version,
            identity: id,
        })
    }

    fn verification_error(error: AppInstallError, message: &str) -> AppInstallError {
        if matches!(
            error.kind,
            AppInstallFailure::Deadline | AppInstallFailure::Cancelled
        ) {
            error
        } else {
            failure(
                AppInstallFailure::Verification,
                format!("{message}: {error}"),
            )
        }
    }

    async fn inventory(
        config: &Config,
        control: &Control,
    ) -> Result<Vec<VerifiedBundle>, AppInstallError> {
        let mut installed = Vec::new();
        for root in [&config.system, &config.user] {
            match fs::symlink_metadata(root) {
                Ok(_) => {
                    directory(root, false)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error(error)),
            }
            let path = root.join(BUNDLE);
            match fs::symlink_metadata(&path) {
                Ok(_) => installed.push(verify(config, &path, None, control).await?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(io_error(error)),
            }
        }
        Ok(installed)
    }

    async fn unchanged_bundle(
        config: &Config,
        app: &VerifiedBundle,
        control: &Control,
    ) -> Result<(), AppInstallError> {
        let confirmed = verify(config, &app.path, None, control).await?;
        if confirmed.identity != app.identity || confirmed.version != app.version {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "App changed since verification",
            ));
        }
        Ok(())
    }

    fn current<'a>(
        installed: &'a [VerifiedBundle],
        target: &Version,
    ) -> Option<&'a VerifiedBundle> {
        installed
            .iter()
            .rev()
            .filter(|app| {
                app.version
                    .as_ref()
                    .is_some_and(|version| version.cmp_precedence(target).is_ge())
            })
            .max_by(|a, b| {
                a.version
                    .as_ref()
                    .unwrap()
                    .cmp_precedence(b.version.as_ref().unwrap())
            })
    }

    fn checksum(bytes: &[u8], asset: &str) -> Result<[u8; 32], AppInstallError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| failure(AppInstallFailure::Verification, "SHA256SUMS is not UTF-8"))?;
        let mut found = None;
        for line in text.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 2 || fields[1].strip_prefix('*').unwrap_or(fields[1]) != asset {
                continue;
            }
            if found.is_some()
                || fields.len() != 2
                || fields[0].len() != 64
                || !fields[0].bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(failure(
                    AppInstallFailure::Verification,
                    "Duplicate or malformed asset checksum",
                ));
            }
            let mut digest = [0; 32];
            for (i, byte) in digest.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&fields[0][i * 2..i * 2 + 2], 16).map_err(|_| {
                    failure(AppInstallFailure::Verification, "Malformed asset checksum")
                })?;
            }
            found = Some(digest);
        }
        found.ok_or_else(|| {
            failure(
                AppInstallFailure::Verification,
                "Target asset missing from SHA256SUMS",
            )
        })
    }

    struct VerifiedDmg(PathBuf);

    async fn download(
        config: &Config,
        target: &Version,
        image: &OwnedPath,
        control: &Control,
    ) -> Result<VerifiedDmg, AppInstallError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    attempt.error("Too many release redirects")
                } else if attempt.url().scheme() != "https" {
                    attempt.error("Insecure release redirect")
                } else if !attempt.url().username().is_empty() || attempt.url().password().is_some()
                {
                    attempt.error("Release redirect contains credentials")
                } else {
                    attempt.follow()
                }
            }))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .referer(false)
            .no_proxy()
            .build()
            .map_err(|error| failure(AppInstallFailure::Download, error))?;
        let asset = format!("Koidrive-{target}-arm64.dmg");
        let base = format!("{}/v{target}", config.release);
        let sums = fetch(
            &client,
            &format!("{base}/SHA256SUMS"),
            1024 * 1024,
            None,
            control,
        )
        .await?;
        let expected = checksum(&sums, &asset)?;
        let path = image.path.clone();
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&path)
            .map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        if identity(&metadata) != image.identity || metadata.nlink() != 1 || !metadata.is_file() {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Download file changed",
            ));
        }
        let digest = fetch(
            &client,
            &format!("{base}/{asset}"),
            config.max_download,
            Some(file),
            control,
        )
        .await?;
        if digest.as_slice() != expected {
            return Err(failure(
                AppInstallFailure::Verification,
                "Downloaded app checksum does not match SHA256SUMS",
            ));
        }
        Ok(VerifiedDmg(path))
    }

    async fn fetch(
        client: &reqwest::Client,
        url: &str,
        limit: u64,
        mut file: Option<File>,
        control: &Control,
    ) -> Result<Vec<u8>, AppInstallError> {
        control
            .bounded(async {
                let mut response = client
                    .get(url)
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status)
                    .map_err(|error| failure(AppInstallFailure::Download, error))?;
                if response.content_length().is_some_and(|len| len > limit) {
                    return Err(failure(
                        AppInstallFailure::Download,
                        "Release asset exceeds download limit",
                    ));
                }
                let mut count = 0u64;
                let mut bytes = Vec::new();
                let mut hash = Sha256::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|error| failure(AppInstallFailure::Download, error))?
                {
                    control.check()?;
                    count = count.saturating_add(chunk.len() as u64);
                    if count > limit {
                        return Err(failure(
                            AppInstallFailure::Download,
                            "Release asset exceeds download limit",
                        ));
                    }
                    if let Some(file) = file.as_mut() {
                        file.write_all(&chunk).map_err(io_error)?;
                        hash.update(&chunk);
                    } else {
                        bytes.extend_from_slice(&chunk);
                    }
                }
                if let Some(file) = file {
                    file.sync_all().map_err(io_error)?;
                    Ok(hash.finalize().to_vec())
                } else {
                    Ok(bytes)
                }
            })
            .await
    }

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
            self.root == other.root
                && self.name == other.name
                && self.root_handle == other.root_handle
        }
    }

    impl Eq for Receipt {}

    fn private_work(path: &Path, create: bool) -> Result<File, AppInstallError> {
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

    fn regular_at(root: &File, name: &OsStr) -> Result<File, AppInstallError> {
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

    fn observed(path: &Path) -> Result<Option<Identity>, AppInstallError> {
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

    fn only_entries(root: &File, allowed: &[&OsStr]) -> Result<(), AppInstallError> {
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
        fn path(&self) -> PathBuf {
            self.root.join(&self.name)
        }

        fn data(&self) -> ReceiptData {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .0
                .clone()
        }

        fn validate_file(
            &self,
            file: &File,
            expected: &ReceiptData,
        ) -> Result<(), AppInstallError> {
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

        fn save(&self, data: &ReceiptData) -> Result<(), AppInstallError> {
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
            let temporary =
                OsString::from(format!(".receipt-write-{}-{nonce}", std::process::id()));
            let root = &self.root_handle.file;
            let mut file = File::from(
                rustix::fs::openat(
                    root,
                    &temporary,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
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

        fn remove(&self) -> Result<(), AppInstallError> {
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

        fn validate(&self, config: &Config) -> Result<(), AppInstallError> {
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

    async fn installed_for(
        receipt: &Receipt,
        config: &Config,
        exact: bool,
        control: &Control,
    ) -> Result<VerifiedBundle, AppInstallError> {
        let data = receipt.data();
        let target = Version::parse(&data.target).map_err(io_error)?;
        if !exact {
            if !matches!(
                data.lifecycle,
                Lifecycle::AwaitingHandoff | Lifecycle::Retiring
            ) {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Incomplete lifecycle requires exact app identity",
                ));
            }
            let installed = inventory(config, control).await?;
            return current(&installed, &target).cloned().ok_or_else(|| {
                failure(
                    AppInstallFailure::Conflict,
                    "Pending transaction has no current release app",
                )
            });
        }
        let stage = data
            .stage
            .as_ref()
            .ok_or_else(|| io_error("Missing app stage"))?;
        let transfer = stage
            .transfer
            .as_ref()
            .ok_or_else(|| io_error("Missing app transfer"))?;
        stage.root.directory()?;
        let path = stage.root.path.join(BUNDLE);
        let expected = transfer.incoming;
        if exact && observed(&path)? != Some(expected) {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Installed app identity changed",
            ));
        }
        let app = verify(config, &path, exact.then_some(&target), control).await?;
        if !app
            .version
            .as_ref()
            .is_some_and(|version| version.cmp_precedence(&target).is_ge())
        {
            return Err(failure(
                AppInstallFailure::Conflict,
                "Pending transaction has no current release app",
            ));
        }
        if exact && app.identity != expected {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Installed app changed during verification",
            ));
        }
        Ok(app)
    }

    async fn capture_ui(
        config: &Config,
        installed: &[VerifiedBundle],
        requested: bool,
        control: &Control,
    ) -> Result<UiIntent, AppInstallError> {
        let mut running = Vec::new();
        for app in installed {
            let output = process(
                &config.osascript,
                &args(&[
                    OsStr::new("-l"),
                    OsStr::new("JavaScript"),
                    OsStr::new("-e"),
                    OsStr::new(INSPECT_RUNNING),
                    app.path.as_os_str(),
                ]),
                control,
            )
            .await?;
            match output.trim_ascii() {
                b"running" => running.push(RunningApp {
                    root: OwnedPath::capture(app.path.parent().unwrap().to_path_buf())?,
                    bundle: OwnedPath {
                        path: app.path.clone(),
                        identity: app.identity,
                    },
                    version: app.version.as_ref().map(ToString::to_string),
                }),
                b"stopped" => (),
                _ => {
                    return Err(failure(
                        AppInstallFailure::InUse,
                        "Cannot inspect running app state",
                    ));
                }
            }
        }
        Ok(UiIntent { requested, running })
    }

    async fn restore_ui(
        config: &Config,
        ui: &UiIntent,
        control: &Control,
    ) -> Result<(), AppInstallError> {
        for running in &ui.running {
            running.root.directory()?;
            running.bundle.directory()?;
            let verified = verify(config, &running.bundle.path, None, control).await?;
            if verified.identity != running.bundle.identity
                || verified.version.as_ref().map(ToString::to_string) != running.version
            {
                return Err(failure(
                    AppInstallFailure::ConcurrentChange,
                    "Previously running app changed",
                ));
            }
            if let AppLaunchResult::Failed(error) =
                open(config, &running.bundle.path, true, control).await
            {
                return Err(failure(AppInstallFailure::Command, error));
            }
        }
        Ok(())
    }

    async fn complete_lifecycle(
        receipt: &Receipt,
        config: &Config,
        control: &Control,
    ) -> Result<AppLaunchResult, AppInstallError> {
        let mut data = receipt.data();
        if matches!(
            data.lifecycle,
            Lifecycle::AwaitingHandoff | Lifecycle::Retiring
        ) {
            installed_for(receipt, config, false, control).await?;
            return Ok(AppLaunchResult::NotRequested);
        }
        let app = installed_for(receipt, config, true, control).await?;
        let mut ui = data.lifecycle.ui();
        let observed_ui = capture_ui(config, std::slice::from_ref(&app), false, control).await?;
        ui.requested |= !observed_ui.running.is_empty();
        data.lifecycle = Lifecycle::Installed(ui.clone());
        receipt.save(&data)?;
        installed_for(receipt, config, true, control).await?;
        let stage = data.stage.as_ref().unwrap();
        let old = stage.parent.path.join(BUNDLE);
        let source = stage.root.path.join(BUNDLE);
        let paths = if source == app.path {
            vec![app.path.as_path(), old.as_path()]
        } else {
            vec![app.path.as_path(), source.as_path(), old.as_path()]
        };
        let stopped = quiesce(config, &paths, control).await;
        installed_for(receipt, config, true, control).await?;
        match stopped {
            Ok(was_running) => ui.requested |= was_running,
            Err(error) => {
                return Ok(AppLaunchResult::Failed(format!(
                    "App installed; launch handoff incomplete: {error}"
                )));
            }
        }
        ui.requested |= !ui.running.is_empty();
        data.lifecycle = Lifecycle::Reopening(ui.clone());
        receipt.save(&data)?;
        installed_for(receipt, config, true, control).await?;
        let launch = open(config, &app.path, ui.requested, control).await;
        installed_for(receipt, config, true, control).await?;
        if !matches!(launch, AppLaunchResult::Failed(_)) {
            data.lifecycle = Lifecycle::AwaitingHandoff;
            receipt.save(&data)?;
        }
        Ok(launch)
    }

    enum Replay {
        Empty,
        Aborted,
        Committed,
    }

    async fn replay_lifecycle(
        receipt: &Receipt,
        config: &Config,
        control: &Control,
    ) -> Result<Replay, AppInstallError> {
        let mut data = receipt.data();
        let Some(stage) = &data.stage else {
            return Ok(Replay::Empty);
        };
        stage.root.directory()?;
        let parent_id = observed(&stage.parent.path)?;
        if parent_id.is_some_and(|id| id != stage.parent.identity) {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Pending staging directory changed",
            ));
        }
        let staged = observed(&stage.parent.path.join(BUNDLE))?;
        let source = observed(&stage.root.path.join(BUNDLE))?;
        let retiring = matches!(data.lifecycle, Lifecycle::Retiring);
        let aborting = matches!(data.lifecycle, Lifecycle::Aborting(_));
        let (before, after) = match &stage.transfer {
            None => (true, false),
            Some(Transfer { incoming, previous }) => {
                let old = previous.as_ref().map(|old| old.identity);
                (
                    staged == Some(*incoming) && source == old,
                    (staged == old && source == Some(*incoming))
                        || (old.is_some() && staged == old && parent_id.is_some())
                        || (retiring && staged.is_none()),
                )
            }
        };
        if aborting || before {
            if !aborting
                && !matches!(
                    data.lifecycle,
                    Lifecycle::Preparing | Lifecycle::Quiescing(_) | Lifecycle::Swapping(_)
                )
            {
                return Err(failure(
                    AppInstallFailure::ConcurrentChange,
                    "Installed transaction reverted unexpectedly",
                ));
            }
            let ui = data.lifecycle.ui();
            restore_ui(config, &ui, control).await?;
            data.lifecycle = Lifecycle::Aborting(ui);
            receipt.save(&data)?;
            return Ok(Replay::Aborted);
        }
        if !after {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Ambiguous app transaction identities",
            ));
        }
        if retiring {
            installed_for(receipt, config, false, control).await?;
        } else if let AppLaunchResult::Failed(error) =
            complete_lifecycle(receipt, config, control).await?
        {
            return Err(failure(AppInstallFailure::Command, error));
        }
        Ok(Replay::Committed)
    }

    async fn recover_receipt(
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
                let current_root =
                    OwnedPath::capture(current.path.parent().unwrap().to_path_buf())?;
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

    async fn refresh_tokens(
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

    async fn recover(
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

    struct OwnedMount {
        point: PathBuf,
        image: PathBuf,
    }

    impl OwnedMount {
        fn check_image(&self, expected: Identity) -> Result<(), AppInstallError> {
            let parent = directory(
                self.image
                    .parent()
                    .ok_or_else(|| io_error("Image has no parent"))?,
                false,
            )?;
            let file = regular_at(
                &parent,
                self.image
                    .file_name()
                    .ok_or_else(|| io_error("Image has no name"))?,
            )?;
            if identity(&file.metadata().map_err(io_error)?) != expected {
                return Err(failure(
                    AppInstallFailure::ConcurrentChange,
                    "Mounted image identity changed",
                ));
            }
            Ok(())
        }

        async fn attach(
            &self,
            config: &Config,
            expected: Identity,
            control: &Control,
        ) -> Result<(), AppInstallError> {
            self.check_image(expected)?;
            process(
                &config.hdiutil,
                &args(&[
                    OsStr::new("attach"),
                    OsStr::new("-readonly"),
                    OsStr::new("-nobrowse"),
                    OsStr::new("-noautoopen"),
                    OsStr::new("-mountpoint"),
                    self.point.as_os_str(),
                    OsStr::new("-plist"),
                    self.image.as_os_str(),
                ]),
                control,
            )
            .await?;
            match self.state(config, control).await? {
                MountState::Owned => Ok(()),
                _ => Err(failure(
                    AppInstallFailure::Verification,
                    "Disk image did not attach at its owned mountpoint",
                )),
            }
        }

        async fn state(
            &self,
            config: &Config,
            control: &Control,
        ) -> Result<MountState, AppInstallError> {
            let info = process(
                &config.hdiutil,
                &args(&[OsStr::new("info"), OsStr::new("-plist")]),
                control,
            )
            .await?;
            let plist = plist::Value::from_reader(std::io::Cursor::new(info)).map_err(io_error)?;
            let images = plist
                .as_dictionary()
                .and_then(|dict| dict.get("images"))
                .and_then(plist::Value::as_array)
                .ok_or_else(|| io_error("Cannot inspect mounted disk images"))?;
            for image in images {
                let Some(dict) = image.as_dictionary() else {
                    return Err(io_error("Invalid disk image inventory"));
                };
                let entities = dict
                    .get("system-entities")
                    .and_then(plist::Value::as_array)
                    .ok_or_else(|| io_error("Invalid disk image entities"))?;
                for entity in entities {
                    let point = entity
                        .as_dictionary()
                        .and_then(|dict| dict.get("mount-point"))
                        .and_then(plist::Value::as_string);
                    if point.map(Path::new) == Some(self.point.as_path()) {
                        return Ok(
                            if dict
                                .get("image-path")
                                .and_then(plist::Value::as_string)
                                .map(Path::new)
                                == Some(self.image.as_path())
                            {
                                MountState::Owned
                            } else {
                                MountState::Foreign
                            },
                        );
                    }
                }
            }
            Ok(MountState::Absent)
        }

        async fn cleanup(
            &self,
            config: &Config,
            expected: Identity,
            control: &Control,
        ) -> Result<(), AppInstallError> {
            match self.state(config, control).await? {
                MountState::Absent => return Ok(()),
                MountState::Foreign => {
                    return Err(failure(
                        AppInstallFailure::UnsafePath,
                        "Unexpected image occupies installer mountpoint",
                    ));
                }
                MountState::Owned => (),
            }
            self.check_image(expected)?;
            process(
                &config.hdiutil,
                &args(&[OsStr::new("detach"), self.point.as_os_str()]),
                control,
            )
            .await?;
            match self.state(config, control).await? {
                MountState::Absent => Ok(()),
                _ => Err(io_error("Installer disk image remains mounted")),
            }
        }
    }

    enum MountState {
        Absent,
        Owned,
        Foreign,
    }

    async fn quiesce(
        config: &Config,
        paths: &[&Path],
        control: &Control,
    ) -> Result<bool, AppInstallError> {
        let mut parameters = args(&[
            OsStr::new("-l"),
            OsStr::new("JavaScript"),
            OsStr::new("-e"),
            OsStr::new(QUIESCE),
        ]);
        parameters.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
        let output = process(&config.osascript, &parameters, control)
            .await
            .map_err(|error| {
                if error.kind == AppInstallFailure::Command {
                    failure(AppInstallFailure::InUse, error)
                } else {
                    error
                }
            })?;
        match output.trim_ascii() {
            b"running" => Ok(true),
            b"stopped" => Ok(false),
            _ => Err(failure(
                AppInstallFailure::InUse,
                "Cannot determine running app state",
            )),
        }
    }

    async fn open(
        config: &Config,
        path: &Path,
        requested: bool,
        control: &Control,
    ) -> AppLaunchResult {
        if !requested {
            return AppLaunchResult::NotRequested;
        }
        match process(
            &config.open,
            &args(&[OsStr::new("-a"), path.as_os_str()]),
            control,
        )
        .await
        {
            Ok(_) => AppLaunchResult::Requested,
            Err(error) => AppLaunchResult::Failed(error.to_string()),
        }
    }

    fn writable(root: &Path) -> Result<bool, AppInstallError> {
        match tempfile::Builder::new()
            .prefix(".koidrive-write-")
            .tempdir_in(root)
        {
            Ok(probe) => {
                probe.close().map_err(io_error)?;
                Ok(true)
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || matches!(error.raw_os_error(), Some(13 | 30 | 1)) =>
            {
                Ok(false)
            }
            Err(error) => Err(io_error(error)),
        }
    }

    fn begin_receipt(
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

    fn older(app: &VerifiedBundle, target: &Version) -> bool {
        app.version
            .as_ref()
            .is_none_or(|version| version.cmp_precedence(target).is_lt())
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

    async fn stage_copy(
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
            &config.ditto,
            &args(&[source.as_os_str(), staged_path.as_os_str()]),
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

    async fn exchange(
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
                &config.codesign,
                &signature_args(&config.updater, SIGNER, false),
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
            if identity(&directory(&app.path, false)?.metadata().map_err(io_error)?) != app.identity
            {
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
            let _root_lock = lock(&root, &work).await.map_err(preserve)?;
            let receipt = begin_receipt(config, work_root, chosen.version.as_ref().unwrap())
                .map_err(preserve)?;
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
        let _user_lock = lock(&work_root, &control).await?;
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
        let (root, root_fd) = if writable(&config.system)? {
            (config.system.clone(), system_root)
        } else {
            (config.user.clone(), directory(&config.user, true)?)
        };
        let _root_lock = lock(&root_fd, &control).await?;
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
            let staged =
                stage_copy(&config, &receipt, &root, &root_fd, &incoming, &control).await?;
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

    #[cfg(test)]
    pub(super) mod tests {
        include!("app_install_tests.rs");
    }
}
