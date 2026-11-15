use std::time::Duration;

use koidrive_driver::Paths;
use koidrive_driver::update::{self, Release};
use koidrive_ipc::{Code, Request, Response};
use semver::Version;

use crate::error::CliError;
use crate::formatting::printable;
use crate::ipc;
use crate::profile::unexpected;
use crate::report::{Done, UpdateCheckData, UpdateNoticeData, UpdateState};

pub(super) const INSTALLER: Duration = Duration::from_secs(5 * 60);
pub(super) const RECONCILE: Duration = Duration::from_secs(7 * 60);

pub(super) fn failed(why: String) -> CliError {
    CliError::new(Code::Failed, why)
}

pub(super) async fn newest(paths: &Paths) -> Result<(Version, String), CliError> {
    let Release { version, url } = update::check_now(paths)
        .await
        .map_err(CliError::from)?
        .ok_or_else(|| failed("GitHub lists no Koidrive release for this system".to_owned()))?;
    let to = Version::parse(&version).map_err(|_| {
        failed(format!(
            "GitHub names the release {version}, which is not a version"
        ))
    })?;
    Ok((to, url))
}

pub(crate) async fn check_updates(paths: &Paths) -> Result<Done, CliError> {
    let update = match ipc::run(paths, Request::CheckForUpdates).await? {
        Response::UpdateChecked { update } => update,
        other => return Err(unexpected(other)),
    };
    let notice = UpdateNoticeData::of(&update, env!("CARGO_PKG_VERSION"), None);
    let text = match (&notice.state, &notice.latest) {
        (_, Some(latest)) => format!(
            "Koidrive {} is out (this device runs {}).\n",
            printable(latest),
            printable(&notice.version)
        ),
        (UpdateState::UpToDate, None) => {
            format!("Koidrive {} is up to date.\n", printable(&notice.version))
        }
        (UpdateState::Off, None) => "Development builds do not check for releases.\n".to_owned(),
        (UpdateState::Available | UpdateState::Unknown, None) => {
            "GitHub has not answered the update check yet.\n".to_owned()
        }
    };
    Ok(Done::ok(UpdateCheckData { update: notice }, text))
}
