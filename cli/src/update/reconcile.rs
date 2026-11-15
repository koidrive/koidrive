use std::path::{Path, PathBuf};

use koidrive_driver::Paths;
use koidrive_ipc::Code;
use semver::Version;
use tokio::process::Command;

use crate::error::CliError;
use crate::report::{Done, UpdateResult, UpdatedData, Via};
use crate::service;
use crate::terminal::Progress;

use super::process::{Child, Echo};
use super::release_source::failed;

pub(super) enum UpdateBinary {
    Other(PathBuf),
    #[cfg(target_os = "macos")]
    App(Box<crate::app_install::VerifiedHelper>),
}

#[derive(Debug)]
pub(super) enum ReconcileError {
    Failed(CliError),
    #[cfg(target_os = "macos")]
    Unverified(crate::app_install::AppInstallError),
}

impl From<CliError> for ReconcileError {
    fn from(error: CliError) -> Self {
        Self::Failed(error)
    }
}

impl UpdateBinary {
    pub(super) fn path(&self) -> &Path {
        match self {
            Self::Other(path) => path,
            #[cfg(target_os = "macos")]
            Self::App(helper) => helper.path(),
        }
    }

    async fn check(&self, deadline: tokio::time::Instant) -> Result<(), ReconcileError> {
        #[cfg(not(target_os = "macos"))]
        let _ = deadline;
        match self {
            Self::Other(_) => Ok(()),
            #[cfg(target_os = "macos")]
            Self::App(helper) => helper
                .check(deadline)
                .await
                .map_err(ReconcileError::Unverified),
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) async fn discover_app_version(
    helper: &crate::app_install::VerifiedHelper,
    deadline: tokio::time::Instant,
) -> Result<Version, ReconcileError> {
    helper
        .check(deadline)
        .await
        .map_err(ReconcileError::Unverified)?;
    let actual = service::version_of(helper.path());
    helper
        .check(deadline)
        .await
        .map_err(ReconcileError::Unverified)?;
    actual.ok_or_else(|| {
        failed(format!(
            "{} did not report a Koidrive version during app discovery",
            helper.path().display()
        ))
        .into()
    })
}

#[cfg(target_os = "macos")]
pub(super) async fn select_app_helper(
    prepared: &crate::app::Prepared,
    existing: crate::app_install::VerifiedHelper,
    from: &Version,
    target: &Version,
    deadline: tokio::time::Instant,
) -> Result<Option<crate::app_install::VerifiedHelper>, crate::app_install::AppInstallError> {
    if let Some(helper) = prepared.verified_helper(deadline).await? {
        return Ok(Some(helper));
    }
    if prepared.skipped_in_ci() && from.cmp_precedence(target).is_ge() {
        existing.check(deadline).await?;
        return Ok(Some(existing));
    }
    Ok(None)
}

pub(super) fn updated(
    from: &Version,
    actual: Version,
    via: Via,
    report: service::Report,
    app: Option<crate::app::Report>,
    changed: bool,
) -> Result<Done, CliError> {
    let changed = actual.cmp_precedence(from).is_gt()
        || changed
        || app.as_ref().is_some_and(|app| !app.committed().is_empty());
    let summary = match changed {
        true => format!("Updated Koidrive from {from} to {actual}."),
        false => format!("Koidrive {actual} is the newest release."),
    };
    let mut text = format!(
        "{summary}\n{}",
        match report.text() {
            text if text.is_empty() => report.status_line(),
            text => text,
        }
    );
    if let Some(app) = &app {
        text.push_str(&app.text());
    }
    let problem = update_problem(&report, app.as_ref());
    let code = match report.retryable() {
        true => Code::Unavailable,
        false => Code::Failed,
    };
    let done = Done::ok(
        UpdatedData {
            from: from.to_string(),
            to: actual.to_string(),
            via: Some(via),
            result: if changed {
                UpdateResult::Updated
            } else {
                UpdateResult::Unchanged
            },
            service: Some(report),
            app,
        },
        text,
    );
    let done = if changed { done } else { done.unchanged() };
    if let Some(reason) = problem {
        return Err(CliError {
            partial: Some(Box::new(done)),
            ..CliError::new(code, reason)
        });
    }
    Ok(done)
}

pub(super) fn update_problem(
    report: &service::Report,
    app: Option<&crate::app::Report>,
) -> Option<String> {
    if report.problem.is_some()
        || matches!(
            report.state,
            service::State::Stopped | service::State::Starting | service::State::Unavailable
        )
    {
        return Some(report.failure_message());
    }
    app.and_then(crate::app::Report::problem).map(str::to_owned)
}

pub(super) async fn finish_reconciliation(
    paths: &Paths,
    prepared: Option<crate::app::Prepared>,
    from: &Version,
    reconciled: Result<(Version, service::Report, Option<crate::app::Report>), ReconcileError>,
    deadline: tokio::time::Instant,
) -> Result<(Version, service::Report, Option<crate::app::Report>), CliError> {
    let (actual, report, forwarded) = match reconciled {
        Ok(reconciled) => reconciled,
        Err(error) => {
            let (mut error, prepared) = match error {
                ReconcileError::Failed(error) => (error, prepared),
                #[cfg(target_os = "macos")]
                ReconcileError::Unverified(error) => {
                    let failure = failed(error.to_string());
                    (failure, prepared.map(|prepared| prepared.invalidate(error)))
                }
            };
            let Some(prepared) = prepared else {
                return Err(error);
            };
            let app = prepared
                .finish(paths, Err(error.message.clone()), deadline)
                .await;
            let text = format!(
                "Koidrive update is incomplete: {}\n{}",
                error.message,
                app.text()
            );
            error.partial = Some(Box::new(app_partial(from, app, text)));
            return Err(error);
        }
    };
    let app = match prepared {
        Some(prepared) => Some(prepared.finish(paths, Ok(&report), deadline).await),
        None => forwarded,
    };
    Ok((actual, report, app))
}

fn app_partial(from: &Version, app: crate::app::Report, text: String) -> Done {
    let committed = app.committed().last();
    let to = match &app {
        crate::app::Report::Present { version, .. } => Version::parse(version).ok(),
        _ => None,
    }
    .or_else(|| committed.map(|commit| commit.version.clone()))
    .unwrap_or_else(|| from.clone());
    let changed = committed.is_some() || to.cmp_precedence(from).is_gt();
    let done = Done::ok(
        UpdatedData {
            from: from.to_string(),
            to: to.to_string(),
            via: Some(Via::App),
            result: if changed {
                UpdateResult::Updated
            } else {
                UpdateResult::Unchanged
            },
            service: None,
            app: Some(app),
        },
        text,
    );
    if changed { done } else { done.unchanged() }
}

pub(super) async fn app_not_updated(
    paths: &Paths,
    from: &Version,
    guidance: &str,
    prepared: crate::app::Prepared,
    deadline: tokio::time::Instant,
) -> CliError {
    let changed = !prepared.committed().is_empty();
    let app = prepared
        .finish(
            paths,
            Err("App installation did not complete".into()),
            deadline,
        )
        .await;
    let message = match &app {
        crate::app::Report::Failed { problem, .. } => format!("{problem}\n{guidance}"),
        crate::app::Report::Skipped { reason } => {
            format!("App installation was skipped ({reason}).\n{guidance}")
        }
        crate::app::Report::Present { .. } => unreachable!("a present app selected its helper"),
    };
    let text = if changed {
        format!("Koidrive update is incomplete.\n{}{guidance}\n", app.text())
    } else {
        format!("Koidrive was not updated.\n{message}\n")
    };
    CliError {
        partial: Some(Box::new(app_partial(from, app, text))),
        ..failed(message)
    }
}

pub(super) const SERVICE_CHECK: &str = "Checking the background service";

pub(super) fn reached(
    koi: &Path,
    actual: Option<Version>,
    target: &Version,
) -> Result<Version, CliError> {
    let actual = actual.ok_or_else(|| {
        failed(format!(
            "{} is missing or did not report a Koidrive version after the update",
            koi.display()
        ))
    })?;
    if actual.cmp_precedence(target).is_lt() {
        return Err(failed(format!(
            "{} is still Koidrive {actual}; the requested release is {target}. Try koi update again when the install source has that release.",
            koi.display()
        )));
    }
    Ok(actual)
}

pub(super) fn checked(
    step: Option<Progress<'_>>,
    finished: &Result<(Version, service::Report, Option<crate::app::Report>), CliError>,
    echo: &mut Echo,
) {
    match finished {
        Ok((_, report, app)) if update_problem(report, app.as_ref()).is_none() => {
            echo.finish(step, "Checked the background service");
        }
        _ => echo.fail(step),
    }
}

pub(super) async fn reconcile_updated(
    binary: &UpdateBinary,
    target: &Version,
    manager: &dyn service::Manager,
    service: Result<service::Service, String>,
    echo: &mut Echo,
    deadline: tokio::time::Instant,
) -> Result<(Version, service::Report, Option<crate::app::Report>), ReconcileError> {
    binary.check(deadline).await?;
    let koi = binary.path();
    let actual = service::version_of(koi);
    binary.check(deadline).await?;
    let actual = reached(koi, actual, target)?;
    let service = service.map_err(failed)?;
    binary.check(deadline).await?;
    let within = deadline.saturating_duration_since(tokio::time::Instant::now());
    let args = ["_reconcile", "--json"];
    let mut command = Command::new(koi);
    command.args(args);
    let output = if within.is_zero() {
        Ok(None)
    } else {
        (echo.run(command, within, Child::Reconcile).await)
            .map(|output| output.map(|(status, printed, _)| (status, printed)))
    };
    let app = output
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .and_then(|(_, printed)| {
            let envelope: serde_json::Value = serde_json::from_slice(printed).ok()?;
            serde_json::from_value::<crate::app::Report>(envelope["data"]["app"].clone()).ok()
        });
    let report = service::delegated_report(manager, &service, koi, &args, within, output);
    Ok((actual, report, app))
}
