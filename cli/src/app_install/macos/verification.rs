use crate::app_install::error::{AppInstallError, AppInstallFailure, failure};
use crate::app_install::macos::config::Config;
use crate::app_install::macos::constants::{BUNDLE, IDENTIFIER, REQUIREMENT};
use crate::app_install::macos::control::{Control, io_error, passed, process};
use crate::app_install::macos::filesystem::{directory, identity, unchanged_directory};
use crate::app_install::macos::state::{PinnedDirectory, VerifiedBundle};
use crate::program::Arg;
use rustix::fs::{Mode, OFlags};
use semver::Version;
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use tokio::sync::watch;
use tokio::time::Instant;

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
pub(super) fn signature_args(
    path: &Path,
    requirement: &str,
    deep: bool,
) -> Result<Vec<Arg>, AppInstallError> {
    let mut arguments = vec![Arg::from("--verify")];
    if deep {
        arguments.push("--deep".into());
    }
    arguments.extend([
        Arg::from("--strict"),
        Arg::from("-R"),
        Arg::from(format!("={requirement}")),
        passed(path)?,
    ]);
    Ok(arguments)
}
pub(super) fn safe_tree(bundle: &Path, control: &Control) -> Result<(), AppInstallError> {
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
pub(in crate::app_install) async fn verify_helper(
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
pub(super) async fn verify(
    config: &Config,
    path: &Path,
    target: Option<&Version>,
    control: &Control,
) -> Result<VerifiedBundle, AppInstallError> {
    safe_tree(path, control)?;
    let id = identity(&fs::symlink_metadata(path).map_err(io_error)?);
    process(
        &config.codesign,
        &signature_args(path, REQUIREMENT, true)?,
        control,
    )
    .await
    .map_err(|error| verification_error(error, "App signature verification failed"))?;
    if target.is_some() {
        let assessment = process(
            &config.spctl,
            &[
                Arg::from("--assess"),
                Arg::from("--type"),
                Arg::from("execute"),
                Arg::from("--verbose=2"),
                passed(path)?,
            ],
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
            let text = value
                .and_then(plist::Value::as_string)
                .ok_or_else(|| failure(version_kind, "App has no valid full KoidriveVersion"))?;
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
pub(super) fn verification_error(error: AppInstallError, message: &str) -> AppInstallError {
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
pub(super) async fn inventory(
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
pub(super) async fn unchanged_bundle(
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
pub(super) fn current<'a>(
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
