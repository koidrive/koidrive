use crate::app_install::error::{AppInstallError, AppInstallFailure, failure};
use crate::app_install::macos::control::io_error;
use std::path::PathBuf;

#[derive(Clone)]
pub(in crate::app_install) struct Config {
    pub(super) system: PathBuf,
    pub(super) user: PathBuf,
    pub(super) work: PathBuf,
    pub(super) release: String,
    pub(super) codesign: PathBuf,
    pub(super) spctl: PathBuf,
    pub(super) hdiutil: PathBuf,
    pub(super) ditto: PathBuf,
    pub(super) osascript: PathBuf,
    pub(super) open: PathBuf,
    pub(super) updater: PathBuf,
    pub(super) max_download: u64,
}
impl Config {
    pub(in crate::app_install) fn production() -> Result<Self, AppInstallError> {
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
