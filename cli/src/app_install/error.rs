#[cfg(target_os = "macos")]
use super::facts::CommitFact;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

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
    Stalled,
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
pub(super) fn failure(kind: AppInstallFailure, message: impl ToString) -> AppInstallError {
    Box::new(AppInstallErrorDetails {
        kind,
        message: message.to_string(),
        residue: Vec::new(),
        committed: Vec::new(),
        retired: Vec::new(),
        preserved: Vec::new(),
    })
}
