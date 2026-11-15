// SPDX-License-Identifier: Apache-2.0

mod error;
mod facts;
mod outcome;

pub(crate) use error::AppInstallError;
#[cfg(target_os = "macos")]
#[expect(unused_imports)]
pub(crate) use error::{AppInstallErrorDetails, AppInstallFailure};
pub(crate) use facts::{AppLaunch, CommitChange, CommitFact};
#[cfg(target_os = "macos")]
pub(crate) use macos::VerifiedHelper;
#[cfg(all(test, target_os = "macos"))]
pub(crate) use macos::tests::{
    pending_retirements_fixture, verified_helper_fixture, verified_helper_fixture_with_metadata,
};
#[cfg(target_os = "macos")]
pub(crate) use outcome::{AppChange, AppLaunchResult, RetiredApp};
pub(crate) use outcome::{AppInstall, AppSkip};
use semver::Version;
use tokio::time::Instant;

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
