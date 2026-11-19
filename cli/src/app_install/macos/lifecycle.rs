use crate::app_install::error::{AppInstallError, AppInstallFailure, failure};
use crate::app_install::macos::config::Config;
use crate::app_install::macos::constants::{BUNDLE, INSPECT_RUNNING, QUIESCE};
use crate::app_install::macos::control::{Control, io_error, passed, process};
use crate::app_install::macos::filesystem::observed;
use crate::app_install::macos::state::{
    Lifecycle, OwnedPath, Receipt, Replay, RunningApp, Transfer, UiIntent, VerifiedBundle,
};
use crate::app_install::macos::verification::{current, inventory, verify};
use crate::app_install::outcome::AppLaunchResult;
use crate::program::Arg;
use semver::Version;
use std::path::Path;

pub(super) async fn installed_for(
    receipt: &Receipt,
    config: &Config,
    exact: bool,
    control: &Control,
) -> Result<VerifiedBundle, AppInstallError> {
    let data = receipt.data();
    let target = Version::parse(&data.target).map_err(io_error)?;
    if !exact {
        if !matches!(
            data.lifecycle,
            Lifecycle::AwaitingHandoff | Lifecycle::Retiring
        ) {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Incomplete lifecycle requires exact app identity",
            ));
        }
        let installed = inventory(config, control).await?;
        return current(&installed, &target).cloned().ok_or_else(|| {
            failure(
                AppInstallFailure::Conflict,
                "Pending transaction has no current release app",
            )
        });
    }
    let stage = data
        .stage
        .as_ref()
        .ok_or_else(|| io_error("Missing app stage"))?;
    let transfer = stage
        .transfer
        .as_ref()
        .ok_or_else(|| io_error("Missing app transfer"))?;
    stage.root.directory()?;
    let path = stage.root.path.join(BUNDLE);
    let expected = transfer.incoming;
    if exact && observed(&path)? != Some(expected) {
        return Err(failure(
            AppInstallFailure::ConcurrentChange,
            "Installed app identity changed",
        ));
    }
    let app = verify(config, &path, exact.then_some(&target), control).await?;
    if !app
        .version
        .as_ref()
        .is_some_and(|version| version.cmp_precedence(&target).is_ge())
    {
        return Err(failure(
            AppInstallFailure::Conflict,
            "Pending transaction has no current release app",
        ));
    }
    if exact && app.identity != expected {
        return Err(failure(
            AppInstallFailure::ConcurrentChange,
            "Installed app changed during verification",
        ));
    }
    Ok(app)
}
pub(super) async fn capture_ui(
    config: &Config,
    installed: &[VerifiedBundle],
    requested: bool,
    control: &Control,
) -> Result<UiIntent, AppInstallError> {
    let mut running = Vec::new();
    for app in installed {
        let output = process(
            &format!("Checking whether {} is running", app.path.display()),
            config.limits.tool,
            &config.osascript,
            &[
                Arg::from("-l"),
                Arg::from("JavaScript"),
                Arg::from("-e"),
                Arg::from(INSPECT_RUNNING),
                passed(&app.path)?,
            ],
            control,
        )
        .await?;
        match output.stdout.trim_ascii() {
            b"running" => running.push(RunningApp {
                root: OwnedPath::capture(app.path.parent().unwrap().to_path_buf())?,
                bundle: OwnedPath {
                    path: app.path.clone(),
                    identity: app.identity,
                },
                version: app.version.as_ref().map(ToString::to_string),
            }),
            b"stopped" => (),
            _ => {
                return Err(failure(
                    AppInstallFailure::InUse,
                    "Cannot inspect running app state",
                ));
            }
        }
    }
    Ok(UiIntent { requested, running })
}
async fn restore_ui(
    config: &Config,
    ui: &UiIntent,
    control: &Control,
) -> Result<(), AppInstallError> {
    for running in &ui.running {
        running.root.directory()?;
        running.bundle.directory()?;
        let verified = verify(config, &running.bundle.path, None, control).await?;
        if verified.identity != running.bundle.identity
            || verified.version.as_ref().map(ToString::to_string) != running.version
        {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Previously running app changed",
            ));
        }
        if let AppLaunchResult::Failed(error) =
            open(config, &running.bundle.path, true, control).await
        {
            return Err(failure(AppInstallFailure::Command, error));
        }
    }
    Ok(())
}
pub(super) async fn complete_lifecycle(
    receipt: &Receipt,
    config: &Config,
    control: &Control,
) -> Result<AppLaunchResult, AppInstallError> {
    let mut data = receipt.data();
    if matches!(
        data.lifecycle,
        Lifecycle::AwaitingHandoff | Lifecycle::Retiring
    ) {
        installed_for(receipt, config, false, control).await?;
        return Ok(AppLaunchResult::NotRequested);
    }
    let app = installed_for(receipt, config, true, control).await?;
    let mut ui = data.lifecycle.ui();
    let observed_ui = capture_ui(config, std::slice::from_ref(&app), false, control).await?;
    ui.requested |= !observed_ui.running.is_empty();
    data.lifecycle = Lifecycle::Installed(ui.clone());
    receipt.save(&data)?;
    installed_for(receipt, config, true, control).await?;
    let stage = data.stage.as_ref().unwrap();
    let old = stage.parent.path.join(BUNDLE);
    let source = stage.root.path.join(BUNDLE);
    let paths = if source == app.path {
        vec![app.path.as_path(), old.as_path()]
    } else {
        vec![app.path.as_path(), source.as_path(), old.as_path()]
    };
    let stopped = quiesce(config, &paths, control).await;
    installed_for(receipt, config, true, control).await?;
    match stopped {
        Ok(was_running) => ui.requested |= was_running,
        Err(error) => {
            return Ok(AppLaunchResult::Failed(format!(
                "App installed; launch handoff incomplete: {error}"
            )));
        }
    }
    ui.requested |= !ui.running.is_empty();
    data.lifecycle = Lifecycle::Reopening(ui.clone());
    receipt.save(&data)?;
    installed_for(receipt, config, true, control).await?;
    let launch = open(config, &app.path, ui.requested, control).await;
    installed_for(receipt, config, true, control).await?;
    if !matches!(launch, AppLaunchResult::Failed(_)) {
        data.lifecycle = Lifecycle::AwaitingHandoff;
        receipt.save(&data)?;
    }
    Ok(launch)
}
pub(super) async fn replay_lifecycle(
    receipt: &Receipt,
    config: &Config,
    control: &Control,
) -> Result<Replay, AppInstallError> {
    let mut data = receipt.data();
    let Some(stage) = &data.stage else {
        return Ok(Replay::Empty);
    };
    stage.root.directory()?;
    let parent_id = observed(&stage.parent.path)?;
    if parent_id.is_some_and(|id| id != stage.parent.identity) {
        return Err(failure(
            AppInstallFailure::ConcurrentChange,
            "Pending staging directory changed",
        ));
    }
    let staged = observed(&stage.parent.path.join(BUNDLE))?;
    let source = observed(&stage.root.path.join(BUNDLE))?;
    let retiring = matches!(data.lifecycle, Lifecycle::Retiring);
    let aborting = matches!(data.lifecycle, Lifecycle::Aborting(_));
    let (before, after) = match &stage.transfer {
        None => (true, false),
        Some(Transfer { incoming, previous }) => {
            let old = previous.as_ref().map(|old| old.identity);
            (
                staged == Some(*incoming) && source == old,
                (staged == old && source == Some(*incoming))
                    || (old.is_some() && staged == old && parent_id.is_some())
                    || (retiring && staged.is_none()),
            )
        }
    };
    if aborting || before {
        if !aborting
            && !matches!(
                data.lifecycle,
                Lifecycle::Preparing | Lifecycle::Quiescing(_) | Lifecycle::Swapping(_)
            )
        {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Installed transaction reverted unexpectedly",
            ));
        }
        let ui = data.lifecycle.ui();
        restore_ui(config, &ui, control).await?;
        data.lifecycle = Lifecycle::Aborting(ui);
        receipt.save(&data)?;
        return Ok(Replay::Aborted);
    }
    if !after {
        return Err(failure(
            AppInstallFailure::ConcurrentChange,
            "Ambiguous app transaction identities",
        ));
    }
    if retiring {
        installed_for(receipt, config, false, control).await?;
    } else if let AppLaunchResult::Failed(error) =
        complete_lifecycle(receipt, config, control).await?
    {
        return Err(failure(AppInstallFailure::Command, error));
    }
    Ok(Replay::Committed)
}
pub(super) async fn quiesce(
    config: &Config,
    paths: &[&Path],
    control: &Control,
) -> Result<bool, AppInstallError> {
    let mut parameters = vec![
        Arg::from("-l"),
        Arg::from("JavaScript"),
        Arg::from("-e"),
        Arg::from(QUIESCE),
    ];
    for path in paths {
        parameters.push(passed(path)?);
    }
    let output = process(
        "Asking Koidrive to quit",
        config.limits.quit,
        &config.osascript,
        &parameters,
        control,
    )
    .await
    .map_err(|error| match error.kind {
        AppInstallFailure::Command => failure(AppInstallFailure::InUse, error),
        AppInstallFailure::Stalled => failure(
            AppInstallFailure::InUse,
            format!(
                "Koidrive did not quit within {} seconds of being asked to",
                config.limits.quit.as_secs()
            ),
        ),
        _ => error,
    })?;
    match output.stdout.trim_ascii() {
        b"running" => Ok(true),
        b"stopped" => Ok(false),
        _ => Err(failure(
            AppInstallFailure::InUse,
            "Cannot determine running app state",
        )),
    }
}
pub(super) async fn open(
    config: &Config,
    path: &Path,
    requested: bool,
    control: &Control,
) -> AppLaunchResult {
    if !requested {
        return AppLaunchResult::NotRequested;
    }
    let launched = match passed(path) {
        Ok(path) => {
            process(
                "Opening Koidrive",
                config.limits.tool,
                &config.open,
                &[Arg::from("-a"), path],
                control,
            )
            .await
        }
        Err(error) => Err(error),
    };
    match launched {
        Ok(_) => AppLaunchResult::Requested,
        Err(error) => AppLaunchResult::Failed(error.to_string()),
    }
}
