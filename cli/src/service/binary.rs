// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use semver::Version;

use super::{Manager, Report, Service, State};

pub(super) const PROBE: Duration = Duration::from_secs(5);

pub const DELEGATED: Duration = Duration::from_secs(7 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Daemon {
    Absent,
    Running(Option<Version>),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub(super) keep: bool,
    pub(super) restart: bool,
}

pub(super) fn target(installed: Option<&Version>, caller: &Version, daemon: &Daemon) -> Target {
    let kept = installed.filter(|installed| installed.cmp_precedence(caller).is_ge());
    let runs = kept.unwrap_or(caller);
    let restart = match daemon {
        Daemon::Absent => false,
        Daemon::Running(None) => true,
        Daemon::Running(Some(version)) => version.cmp_precedence(runs).is_lt(),
    };
    Target {
        keep: kept.is_some(),
        restart,
    }
}

fn output_within(
    command: &mut Command,
    within: Duration,
) -> io::Result<Option<(ExitStatus, Vec<u8>)>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (sent, read) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut printed = Vec::new();
        let _ = sent.send(stdout.read_to_end(&mut printed).map(|_| printed));
    });
    let deadline = Instant::now() + within;
    let printed = read.recv_timeout(within).ok().and_then(Result::ok);
    let status = loop {
        match child.try_wait() {
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(status) => break status,
            Err(_) => break None,
        }
    };
    let (Some(printed), Some(status)) = (printed, status) else {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(None);
    };
    Ok(Some((status, printed)))
}

pub fn version_of(binary: &Path) -> Option<Version> {
    let mut command = crate::program::found(binary).ok()?;
    command.arg("--version").stderr(Stdio::null());
    let (status, printed) = output_within(&mut command, PROBE).ok()??;
    if !status.success() {
        return None;
    }
    let printed = String::from_utf8(printed).ok()?;
    Version::parse(printed.strip_prefix("koi ")?.trim()).ok()
}

pub(super) fn volatile_binary(binary: &Path) -> bool {
    let volatile = |path: &Path| {
        path.starts_with("/Volumes")
            || path
                .components()
                .any(|part| part.as_os_str() == "AppTranslocation")
    };
    volatile(binary) || fs::canonicalize(binary).is_ok_and(|path| volatile(&path))
}

pub(super) const MOVE_TO_APPLICATIONS: &str =
    "Move Koidrive to Applications, then open it again to enable background sync.";

pub(super) fn newer_installed(manager: &dyn Manager, service: &Service) -> Option<PathBuf> {
    let binary = PathBuf::from(manager.installed(service)?);
    if volatile_binary(&binary) {
        return None;
    }
    let version = version_of(&binary)?;
    version
        .cmp_precedence(&this_version())
        .is_gt()
        .then_some(binary)
}

pub(super) async fn delegated(
    manager: &dyn Manager,
    service: &Service,
    binary: &Path,
    asked: bool,
    within: Duration,
) -> Report {
    let args: &[&str] = match asked {
        true => &["service", "start", "--json"],
        false => &["_reconcile", "--no-app", "--json"],
    };
    let output = match crate::program::found_async(binary) {
        Ok(mut command) => {
            command.args(args).stderr(Stdio::inherit());
            delegated_output(&mut command, within).await
        }
        Err(error) => Err(error),
    };
    delegated_report(manager, service, binary, args, within, output)
}

async fn delegated_output(
    command: &mut tokio::process::Command,
    within: Duration,
) -> io::Result<Option<(ExitStatus, Vec<u8>)>> {
    use tokio::io::AsyncReadExt;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut printed = Vec::new();
    let output = tokio::time::timeout(within, async {
        tokio::try_join!(child.wait(), stdout.read_to_end(&mut printed))
    })
    .await;
    match output {
        Ok(Ok((status, _))) => Ok(Some((status, printed))),
        failed => {
            child.kill().await?;
            match failed {
                Ok(Err(error)) => Err(error),
                _ => Ok(None),
            }
        }
    }
}

pub fn delegated_report(
    manager: &dyn Manager,
    service: &Service,
    binary: &Path,
    args: &[&str],
    within: Duration,
    output: io::Result<Option<(ExitStatus, Vec<u8>)>>,
) -> Report {
    let failed = |why: String| {
        let problem = format!(
            "{} {} {why}",
            binary.display(),
            args[..args.len() - 1].join(" ")
        );
        Report::troubled(manager, Some(service), problem)
    };
    let (status, printed) = match output {
        Ok(Some(output)) => output,
        Ok(None) => {
            return failed(format!(
                "did not finish within {} seconds",
                within.as_secs()
            ));
        }
        Err(error) => return failed(format!("did not run: {error}")),
    };
    let envelope: serde_json::Value = serde_json::from_slice(&printed).unwrap_or_default();
    let reported = &envelope["data"]["service"];
    let mut report = match serde_json::from_value::<Report>(reported.clone()) {
        Ok(report) => report,
        Err(_) => match envelope["error"]["message"].as_str() {
            Some(message) => return failed(format!("failed: {message}")),
            None if status.success() && reported.is_object() => {
                Report::new(manager, manager.name(service), State::Running)
            }
            None => return failed(format!("reported no service ({status})")),
        },
    };
    report.log = service.log(manager.kind());
    report
}

pub fn this_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate's version is SemVer")
}

pub(super) fn start(
    manager: &dyn Manager,
    service: &Service,
    daemon: &Daemon,
) -> Result<(), String> {
    let installed = manager
        .installed(service)
        .filter(|binary| !volatile_binary(Path::new(binary)));
    let volatile = volatile_binary(Path::new(&service.binary));
    if volatile && installed.is_none() {
        return Err(MOVE_TO_APPLICATIONS.to_owned());
    }
    let version = (installed.as_deref()).and_then(|binary| version_of(Path::new(binary)));
    let caller = this_version();
    let caller = match volatile {
        true => version.as_ref().unwrap_or(&caller),
        false => &caller,
    };
    let target = target(version.as_ref(), caller, daemon);
    match (target.keep || volatile, installed) {
        (true, Some(binary)) => manager.start(
            &Service {
                binary,
                ..service.clone()
            },
            target.restart,
        ),
        _ => manager.start(service, target.restart),
    }
}
