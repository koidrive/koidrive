use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use koidrive_driver::Paths;

use crate::error::CliError;
use crate::report::{Done, Via};
use crate::service;

use super::inventory::{Install, Method, command_text};
use super::process::{Child, Echo, Steps, exit, minutes};
use super::reconcile::{
    SERVICE_CHECK, UpdateBinary, checked, finish_reconciliation, reconcile_updated, updated,
};
use super::release_source::{INSTALLER, RECONCILE, failed, newest};

const TAP_REFRESH: Duration = Duration::from_secs(60);
const TAP: &str = "koidrive/tap";

pub(super) async fn homebrew(
    paths: &Paths,
    install: &Install,
    brew: &Path,
    echo: &mut Echo,
    steps: &Steps,
) -> Result<Done, CliError> {
    let command = install.fallback().ok_or_else(|| {
        failed("Homebrew's path cannot be represented as a shell command".to_owned())
    })?;
    let guidance = format!("To update, run: {}", command_text(&command));
    let koi = match install.method {
        Method::Formula { .. } => brew
            .parent()
            .and_then(Path::parent)
            .map(|prefix| prefix.join("opt/koidrive/bin/koi"))
            .unwrap_or_else(|| install.binary.clone()),
        _ => install.binary.clone(),
    };
    let from = service::version_of(&koi).unwrap_or_else(service::this_version);
    let (to, _) = newest(paths).await?;
    let (mut refresh, mut failure) = (None, None);
    if to.cmp_precedence(&from).is_gt() {
        if !fs::metadata(brew)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        {
            return Err(failed(format!(
                "Homebrew is not at {}. {guidance}",
                brew.display()
            )));
        }
        let step = steps.still("Updating Koidrive with Homebrew");
        if let Err(why) = refresh_tap(brew, echo).await {
            echo.line(&why, true);
            refresh = Some(why);
        }
        let upgraded = match crate::program::found_async(brew) {
            Ok(mut upgrade) => {
                upgrade
                    .args(&command[1..])
                    .env("HOMEBREW_NO_AUTO_UPDATE", "1")
                    .env_remove("HOMEBREW_ASK");
                echo.run(upgrade, INSTALLER, Child::Progress).await
            }
            Err(error) => Err(error),
        };
        match upgraded {
            Ok(Some((status, _, _))) if status.success() => (),
            Ok(Some((status, _, _))) => {
                failure = Some(format!("brew upgrade failed ({})", exit(status)))
            }
            Ok(None) => {
                failure = Some(format!(
                    "brew upgrade did not finish within {} minutes",
                    minutes(INSTALLER)
                ))
            }
            Err(error) => failure = Some(format!("{} did not run: {error}", brew.display())),
        }
        match (&failure, service::version_of(&koi)) {
            (None, Some(now)) if now.cmp_precedence(&to).is_ge() => {
                echo.finish(step, &format!("Homebrew installed Koidrive {now}"));
            }
            _ => echo.fail(step),
        }
    }
    if let Some(failure) = &failure
        && service::version_of(&koi).is_none_or(|version| version == from)
    {
        return Err(failed(format!("{failure}. {guidance}")));
    }
    let deadline = tokio::time::Instant::now() + RECONCILE;
    let step = steps.start(SERVICE_CHECK);
    let reconciled = reconcile_updated(
        &UpdateBinary::Other(koi),
        &from,
        service::system().as_ref(),
        service::this_service(paths),
        echo,
        deadline,
    )
    .await;
    let finished = finish_reconciliation(paths, None, &from, reconciled, deadline).await;
    checked(step, &finished, echo);
    let (actual, report, app) = finished?;
    let incomplete = actual.cmp_precedence(&to).is_lt();
    let (mut done, service) =
        match updated(&from, actual.clone(), Via::Homebrew, report, app, false) {
            Ok(done) => (done, None),
            Err(CliError {
                partial: Some(partial),
                code,
                message,
                ..
            }) => (*partial, Some(CliError::new(code, message))),
            Err(error) => return Err(error),
        };
    let message = match (incomplete, failure, service) {
        (false, None, None) => return Ok(done),
        (false, None, Some(service)) => {
            return Err(CliError {
                partial: Some(Box::new(done)),
                ..service
            });
        }
        (false, Some(failure), _) => {
            format!("{failure} after installing Koidrive {actual}. {guidance}")
        }
        (true, failure, _) => {
            let why = failure
                .or(refresh)
                .unwrap_or_else(|| format!("The {TAP} tap does not have Koidrive {to} yet"));
            let service = done.text.split_once('\n').map_or("", |(_, rest)| rest);
            done.text = format!(
                "Koidrive update is incomplete: Homebrew has {actual}, and the newest release is {to}.\n{service}"
            );
            format!(
                "Koidrive update is incomplete: the newest release is {to}, and Homebrew has {actual}. {why}. {guidance}"
            )
        }
    };
    Err(CliError {
        partial: Some(Box::new(done)),
        ..failed(message)
    })
}

async fn refresh_tap(brew: &Path, echo: &mut Echo) -> Result<(), String> {
    let mut repository = crate::program::found_async(brew).map_err(|error| error.to_string())?;
    repository
        .args(["--repository", TAP])
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(TAP_REFRESH, repository.output()).await;
    let tap = match &output {
        Ok(Ok(output)) if output.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
        }
        _ => PathBuf::new(),
    };
    if !tap.is_absolute() || !tap.join(".git").exists() {
        return Err(format!(
            "Homebrew did not name a git checkout of the {TAP} tap"
        ));
    }
    let homebrew_git = brew.with_file_name("git");
    let git = match homebrew_git.is_file() {
        true => homebrew_git,
        false => PathBuf::from("/usr/bin/git"),
    };
    let mut pull = crate::program::found_async(&git).map_err(|error| error.to_string())?;
    pull.arg("-C")
        .arg(crate::program::Confined::new(&tap).map_err(|error| error.to_string())?)
        .args(["pull", "--ff-only", "--quiet"])
        .env("GIT_TERMINAL_PROMPT", "0");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_PREFIX",
    ] {
        pull.env_remove(variable);
    }
    match echo.run(pull, TAP_REFRESH, Child::Progress).await {
        Ok(Some((status, _, _))) if status.success() => Ok(()),
        Ok(Some((status, _, _))) => Err(format!(
            "Could not refresh the {TAP} tap at {} (git {})",
            tap.display(),
            exit(status)
        )),
        Ok(None) => Err(format!(
            "Could not refresh the {TAP} tap at {} within {} seconds",
            tap.display(),
            TAP_REFRESH.as_secs()
        )),
        Err(error) => Err(format!(
            "Could not refresh the {TAP} tap at {}: {error}",
            tap.display()
        )),
    }
}
