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
pub(crate) use macos::{QUIESCE, writable};

#[cfg(target_os = "macos")]
mod macos;
