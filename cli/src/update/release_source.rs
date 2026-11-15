use std::time::Duration;

use koidrive_driver::Paths;
use koidrive_driver::update::{self, Release};
use koidrive_ipc::Code;
use semver::Version;

use crate::error::CliError;

pub(super) const INSTALLER: Duration = Duration::from_secs(5 * 60);
pub(super) const RECONCILE: Duration = Duration::from_secs(7 * 60);

pub(super) fn failed(why: String) -> CliError {
    CliError::new(Code::Failed, why)
}

pub(super) async fn newest(paths: &Paths) -> Result<(Version, String), CliError> {
    let Release { version, url } = update::check_now(paths)
        .await
        .map_err(|error| CliError::new(Code::Unavailable, error.to_string()))?
        .ok_or_else(|| failed("GitHub lists no Koidrive release for this system".to_owned()))?;
    let to = Version::parse(&version).map_err(|_| {
        failed(format!(
            "GitHub names the release {version}, which is not a version"
        ))
    })?;
    Ok((to, url))
}
