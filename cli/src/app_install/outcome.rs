#[cfg(target_os = "macos")]
use super::facts::CommitFact;
#[cfg(target_os = "macos")]
use super::macos::state as macos;
#[cfg(target_os = "macos")]
use semver::Version;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

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
    pub(super) ownership: macos::RetiredOwnership,
}
