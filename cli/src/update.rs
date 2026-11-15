// SPDX-License-Identifier: Apache-2.0
#[cfg(target_os = "macos")]
use std::fs;

use koidrive_driver::Paths;
use koidrive_driver::update;
use koidrive_ipc::Next;

use crate::error::CliError;
use crate::report::{Done, UpdateResult, UpdatedData};
use crate::service;

mod handoff;
mod homebrew;
mod installer;
mod inventory;
mod process;
mod reconcile;
mod release_source;

pub use handoff::follow_newest;
pub use inventory::homebrew_link;
pub(crate) use inventory::{Install, Method, on_path};
pub(crate) use release_source::check_updates;

use homebrew::homebrew;
use installer::{installer, script_url};
use inventory::command_text;
use process::{Echo, Steps};
#[cfg(target_os = "macos")]
use reconcile::{ReconcileError, discover_app_version, select_app_helper};
use reconcile::{
    SERVICE_CHECK, UpdateBinary, app_not_updated, checked, finish_reconciliation, reached,
    reconcile_updated, updated,
};
use release_source::{RECONCILE, failed, newest};

fn refuse_root(root: bool) -> Result<(), CliError> {
    match root {
        true => Err(CliError::usage(
            "koi update upgrades Koidrive for one user; run it as your own user, not as root",
        )),
        false => Ok(()),
    }
}

pub async fn run(paths: &Paths, json: bool, verbose: bool) -> Result<Done, CliError> {
    refuse_root(rustix::process::geteuid().is_root())?;
    let install = Install::current()?;
    match &install.method {
        Method::Development => {
            return Ok(Done::ok(
                UpdatedData {
                    from: env!("CARGO_PKG_VERSION").to_owned(),
                    to: env!("CARGO_PKG_VERSION").to_owned(),
                    via: None,
                    app: None,
                    result: UpdateResult::Unchanged,
                    service: None,
                },
                "Development builds do not update to releases.\n".to_owned(),
            )
            .unchanged());
        }
        Method::AppDownload => return download(paths, &install).await,
        _ => {}
    }
    let mut echo = Echo::open(&update::log_path(paths), json, verbose);
    let steps = Steps::new(json, verbose);
    let updated = match &install.method {
        Method::Formula { brew } | Method::CaskElsewhere { brew } => {
            homebrew(paths, &install, brew, &mut echo, &steps).await
        }
        _ => release(paths, &install, &mut echo, &steps).await,
    };
    updated.map_err(|mut error| {
        echo.log(&error.message);
        if let Some(command) = install.fallback() {
            error.next.push(Next::of(&command));
        }
        error
    })
}

async fn download(paths: &Paths, install: &Install) -> Result<Done, CliError> {
    let (to, url) = newest(paths).await?;
    let from = service::this_version();
    if to.cmp_precedence(&from).is_gt() {
        return Err(failed(format!("Download Koidrive {to} from {url}")));
    }
    Ok(Done::ok(
        UpdatedData {
            from: from.to_string(),
            to: from.to_string(),
            via: install.method.via(),
            app: None,
            result: UpdateResult::Unchanged,
            service: None,
        },
        format!("Koidrive {from} is the newest release.\n"),
    )
    .unchanged())
}

async fn release(
    paths: &Paths,
    install: &Install,
    echo: &mut Echo,
    steps: &Steps,
) -> Result<Done, CliError> {
    let (to, url) = newest(paths).await?;
    let guidance = match install.fallback() {
        Some(command) => format!("To update, run: {}", command_text(&command)),
        None => format!("Download Koidrive {to} from {url}"),
    };
    #[cfg(target_os = "macos")]
    if let Method::Cask { .. } = install.method
        && let Some(folder) = install.binary.ancestors().nth(4)
        && !crate::app_install::writable(folder).map_err(|error| failed(error.to_string()))?
    {
        return Err(failed(format!(
            "This user cannot write {}, so Koidrive cannot replace the Homebrew app there.\n{guidance}",
            folder.display()
        )));
    }
    let manager = service::system();
    let service = service::this_service(paths);
    let app_deadline = tokio::time::Instant::now() + RECONCILE;
    #[cfg(target_os = "macos")]
    let existing_app = if matches!(install.method, Method::App | Method::Cask { .. }) {
        let binary = &install.binary;
        let discovered = async {
            let helper = crate::app_install::verify_existing(binary, app_deadline)
                .await
                .map_err(ReconcileError::Unverified)?;
            let version = discover_app_version(&helper, app_deadline).await?;
            Ok::<_, ReconcileError>((helper, version))
        }
        .await;
        Some(discovered.map_err(|error| {
            let mut error = match error {
                ReconcileError::Failed(error) => error,
                ReconcileError::Unverified(error) => failed(error.to_string()),
            };
            error.message.push_str(&format!("\n{guidance}"));
            error
        })?)
    } else {
        None
    };
    let from = match install.method {
        #[cfg(target_os = "macos")]
        Method::App | Method::Cask { .. } => existing_app.as_ref().unwrap().1.clone(),
        #[cfg(not(target_os = "macos"))]
        Method::App | Method::Cask { .. } => service::this_version(),
        _ => service::version_of(&install.binary).unwrap_or_else(service::this_version),
    };
    let upgrading = to.cmp_precedence(&from).is_gt();
    let koi = match install.method {
        Method::App | Method::Cask { .. } => None,
        _ => Some(UpdateBinary::Other(install.binary.clone())),
    };
    #[cfg(target_os = "macos")]
    let mut koi = koi;
    let prepared_app: Option<crate::app::Prepared> = None;
    #[cfg(target_os = "macos")]
    let mut prepared_app = prepared_app;
    match install.method {
        Method::App | Method::Cask { .. } => {
            #[cfg(target_os = "macos")]
            let step = steps.start(&format!("Downloading Koidrive {to}"));
            let prepared =
                crate::app::prepare(&to, crate::app_install::AppLaunch::Preserve, app_deadline)
                    .await;
            #[cfg(target_os = "macos")]
            {
                match select_app_helper(
                    &prepared,
                    existing_app.unwrap().0,
                    &from,
                    &to,
                    app_deadline,
                )
                .await
                {
                    Ok(Some(helper))
                        if !matches!(install.method, Method::Cask { .. })
                            || fs::canonicalize(helper.path())
                                .is_ok_and(|path| path == install.binary) =>
                    {
                        koi = Some(UpdateBinary::App(Box::new(helper)))
                    }
                    Ok(Some(installed)) => {
                        let cask =
                            crate::app_install::verify_existing(&install.binary, app_deadline)
                                .await
                                .and_then(|cask| {
                                    cask.require_version(&to.to_string()).map(|()| cask)
                                });
                        match cask {
                            Ok(cask) => koi = Some(UpdateBinary::App(Box::new(cask))),
                            Err(mut error) => {
                                error.message = format!(
                                    "Koidrive {to} went to {}, but the Homebrew app at {} is not Koidrive {to}",
                                    installed.path().display(),
                                    install.binary.display()
                                );
                                return Err(app_not_updated(
                                    paths,
                                    &from,
                                    &guidance,
                                    prepared.invalidate(error),
                                    app_deadline,
                                )
                                .await);
                            }
                        }
                    }
                    Ok(None) => {
                        return Err(app_not_updated(
                            paths,
                            &from,
                            &guidance,
                            prepared,
                            app_deadline,
                        )
                        .await);
                    }
                    Err(error) => {
                        return Err(app_not_updated(
                            paths,
                            &from,
                            &guidance,
                            prepared.invalidate(error),
                            app_deadline,
                        )
                        .await);
                    }
                }
                prepared_app = Some(prepared);
                echo.finish(step, &format!("Downloaded and checked Koidrive {to}"));
            }
            #[cfg(not(target_os = "macos"))]
            return Err(app_not_updated(paths, &from, &guidance, prepared, app_deadline).await);
        }
        Method::Installer if upgrading => {
            let dir = (install.binary.parent()).ok_or_else(|| failed(guidance.clone()))?;
            let source = script_url()?;
            let step = steps.start(&format!("Downloading Koidrive {to}"));
            let installed = installer(source, dir, &to, echo)
                .await
                .and_then(|()| reached(&install.binary, service::version_of(&install.binary), &to));
            if let Err(error) = installed {
                echo.fail(step);
                return Err(error);
            }
            echo.finish(
                step,
                &format!("Downloaded, checked and installed Koidrive {to}"),
            );
        }
        Method::Installer => (),
        Method::CaskElsewhere { .. }
        | Method::Formula { .. }
        | Method::AppDownload
        | Method::Development => unreachable!(),
    }
    let koi = koi.expect("an installation has a koi path");
    let changed = prepared_app
        .as_ref()
        .map_or(upgrading, |app| !app.committed().is_empty());
    let reconcile_deadline = if prepared_app.is_some() {
        app_deadline
    } else {
        tokio::time::Instant::now() + RECONCILE
    };
    let step = steps.start(SERVICE_CHECK);
    let reconciled = reconcile_updated(
        &koi,
        &to,
        manager.as_ref(),
        service,
        echo,
        reconcile_deadline,
    )
    .await;
    let finished =
        finish_reconciliation(paths, prepared_app, &from, reconciled, app_deadline).await;
    checked(step, &finished, echo);
    let (actual, report, app) = finished?;
    updated(
        &from,
        actual,
        install
            .method
            .via()
            .expect("a release installation has a source"),
        report,
        app,
        changed,
    )
}

#[cfg(test)]
#[path = "../tests/support/executable.rs"]
mod fixture;

#[cfg(test)]
mod tests;
