use crate::app_install::error::AppInstallError;
use crate::app_install::facts::CommitFact;
use crate::app_install::outcome::RetiredApp;
use std::fs;
use std::path::PathBuf;

#[derive(Default)]
pub(super) struct Attempt {
    pub(super) committed: Vec<CommitFact>,
    pub(super) retired: Vec<PathBuf>,
    pub(super) preserved: Vec<PathBuf>,
    pub(super) receipts: Vec<PathBuf>,
}
impl Attempt {
    pub(super) fn retain(&mut self, token: &RetiredApp) {
        self.retired.push(token.bundle.clone());
        self.receipts.push(token.ownership.receipt.path());
    }

    pub(super) fn fail(self, mut error: AppInstallError) -> AppInstallError {
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
