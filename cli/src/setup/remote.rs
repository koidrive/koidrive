// SPDX-License-Identifier: Apache-2.0

use std::time::{Duration, Instant};

use koidrive_net::{Api, ApiError};
use koidrive_proto::v1::ServerInfo;
use url::Url;

#[cfg(feature = "test-setup")]
use super::testing;
use crate::error::CliError;

/// How long a new or upgraded Worker may take to answer, and to report its new version.
const SERVE_WAIT: Duration = Duration::from_secs(5 * 60);

/// The Koidrive API of the server at `url`. Tests serve it over http on a loopback address.
pub(super) fn server_api(url: &Url) -> Result<Api, ApiError> {
    #[cfg(feature = "test-setup")]
    if let Some(testing) = testing::config().map_err(ApiError::Url)?
        && url.origin() == testing.base.origin()
    {
        return Api::local_development(url.clone());
    }
    if cfg!(test) {
        Api::local_development(url.clone())
    } else {
        Api::new(url.clone())
    }
}

pub(super) async fn server_info(url: &Url) -> Option<ServerInfo> {
    let api = server_api(url).ok()?;
    tokio::time::timeout(Duration::from_secs(10), api.server_info())
        .await
        .ok()?
        .ok()
}

/// Polls `GET /v1/server` until it answers, and until it reports `version` when one is given.
pub(super) async fn wait_until_serving(
    url: &Url,
    version: Option<&str>,
) -> Result<ServerInfo, CliError> {
    let started = Instant::now();
    loop {
        if let Some(info) = server_info(url)
            .await
            .filter(|info| version.is_none_or(|version| info.server_version == version))
        {
            return Ok(info);
        }
        if started.elapsed() > SERVE_WAIT {
            return Err(format!(
                "{url} did not answer as expected within {} seconds",
                SERVE_WAIT.as_secs()
            )
            .into());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Polls until the Account DO reports `setup_attempts`, which can take up to 300 seconds after an upgrade.
pub(super) async fn wait_for_setup_attempts(url: &Url) -> Result<(), CliError> {
    let started = Instant::now();
    loop {
        if server_info(url)
            .await
            .is_some_and(|info| info.setup_attempts)
        {
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(330) {
            return Err("the server did not move to the new version within 330 seconds; run the command again".into());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
