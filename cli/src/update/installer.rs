use std::io::Write;
use std::path::Path;
use std::time::Duration;

use koidrive_ipc::Code;
use semver::Version;
use tokio::process::Command;
use url::Url;

use crate::error::CliError;

use super::process::{Child, Echo, exit, minutes};
use super::release_source::{INSTALLER, failed};

const INSTALL_SCRIPT: &str = "https://koidrive.com/install.sh";
const INSTALL_SCRIPT_VARIABLE: &str = "KOIDRIVE_INSTALL_SCRIPT_URL";

pub(super) fn script_url() -> Result<(Url, bool), CliError> {
    let text = std::env::var(INSTALL_SCRIPT_VARIABLE).unwrap_or_else(|_| INSTALL_SCRIPT.to_owned());
    let url = Url::parse(&text).map_err(|_| failed(format!("{text} is not a URL")))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    match url.scheme() {
        "https" => Ok((url, true)),
        "http" if loopback => Ok((url, false)),
        _ => Err(failed(format!("{text} is not an https URL"))),
    }
}

pub(super) async fn installer(
    (url, https): (Url, bool),
    dir: &Path,
    version: &Version,
    echo: &mut Echo,
) -> Result<(), CliError> {
    let unreachable = |error: reqwest::Error| {
        CliError::new(
            Code::Unavailable,
            format!("install.sh cannot be downloaded from {url}: {error}"),
        )
    };
    let http = reqwest::Client::builder()
        .https_only(https)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(unreachable)?;
    let script = (http.get(url.clone()).send().await)
        .and_then(reqwest::Response::error_for_status)
        .map_err(unreachable)?
        .bytes()
        .await
        .map_err(unreachable)?;
    let file = tempfile::Builder::new()
        .prefix("koidrive-install")
        .suffix(".sh")
        .tempfile()
        .and_then(|mut file| file.write_all(&script).map(|()| file))
        .map_err(|error| failed(format!("install.sh cannot be saved: {error}")))?;
    let mut command = Command::new("/bin/sh");
    command
        .arg(file.path())
        .env("KOIDRIVE_INSTALL_DIR", dir)
        .env("KOIDRIVE_VERSION", format!("v{version}"));
    let (status, _, _) = (echo.run(command, INSTALLER, Child::Progress).await)
        .map_err(|error| failed(format!("install.sh did not run: {error}")))?
        .ok_or_else(|| {
            failed(format!(
                "install.sh did not finish within {} minutes",
                minutes(INSTALLER)
            ))
        })?;
    match status.success() {
        true => Ok(()),
        false => Err(failed(format!("install.sh failed ({})", exit(status)))),
    }
}
