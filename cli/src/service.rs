// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use koidrive_driver::{Config, Flock, Paths, Status};
use koidrive_ipc::{Blocked, Request, Response};
use schemars::JsonSchema;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::ipc;
use crate::lifeline::{INVENTORY, LIFELINE};

mod binary;
mod command;
mod definition;
mod launchd;
mod report;
mod systemd;

pub use binary::{DELEGATED, Daemon, delegated_report, this_version, version_of};
pub use definition::*;
pub use launchd::Launchd;
pub use report::Report;
pub use systemd::Systemd;

use binary::{delegated, newer_installed, start};

const UNIT: &str = include_str!("../systemd/koidrive.service");

/// How long a started service may take to show as running. launchd spawns a job a moment after `launchctl bootstrap`
/// returns, so the first read can still find it not running.
const SPAWN: Duration = Duration::from_secs(5);

const LOCK_WAIT: Duration = Duration::from_secs(60);

/// What the service manager reports for a service, or what keeps koi from using it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    /// The service manager is about to start the daemon again, such as launchd waiting out its restart throttle.
    Starting,
    /// Installed, but not running: it failed.
    Stopped,
    /// The launch agent is installed but launchd lists it as disabled: the person switched koi off in Login Items.
    SwitchedOff,
    NotInstalled,
    /// This machine has no service manager koi can use, such as a Linux system without a systemd user session.
    Unavailable,
    /// config.toml turns the service off.
    Off,
}

pub trait Manager {
    fn kind(&self) -> ServiceManager;
    fn name(&self, service: &Service) -> String;
    fn state(&self, service: &Service) -> Result<State, String>;
    fn installed(&self, service: &Service) -> Option<String>;
    fn start(&self, service: &Service, restart: bool) -> Result<(), String>;
    fn remove(&self, service: &Service) -> Result<(), String>;
    fn crashed(&self, _service: &Service) -> Option<String> {
        None
    }
    fn lingers(&self) -> Option<bool> {
        None
    }
    fn linger(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Remove,
    Leave,
}

pub fn plan(folders: usize, state: State, asked: bool) -> Action {
    match (folders, state) {
        (_, State::Unavailable | State::Off) => Action::Leave,
        (0, State::NotInstalled) => Action::Leave,
        (0, _) => Action::Remove,
        (_, State::Starting) => Action::Leave,
        (_, State::SwitchedOff) if !asked => Action::Leave,
        _ => Action::Start,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceManager {
    Systemd,
    Launchd,
}

pub async fn converge(
    manager: &dyn Manager,
    service: &Service,
    folders: usize,
    off: bool,
    asked: bool,
    daemon: &Daemon,
) -> Report {
    let name = manager.name(service);
    let mut report = match manager.state(service) {
        _ if off => Report::new(manager, name, State::Off),
        Err(problem) => Report::unavailable(manager, problem),
        Ok(found) => {
            let mut before = found;
            let settled = Instant::now() + ipc::STARTING;
            while before == State::Starting && Instant::now() < settled {
                tokio::time::sleep(Duration::from_millis(100)).await;
                before = manager.state(service).unwrap_or(before);
            }
            let action = plan(folders, before, asked);
            let daemon = match found {
                State::Running => daemon,
                _ => &Daemon::Absent,
            };
            let outcome = match action {
                Action::Leave => Ok(()),
                Action::Start => start(manager, service, daemon),
                Action::Remove => manager.remove(service),
            };
            let mut after = manager.state(service).unwrap_or(before);
            let waiting = Instant::now();
            let budget = |after| match (before, after) {
                (State::Starting, _) => Duration::ZERO,
                (_, State::Starting) => ipc::STARTING,
                _ => SPAWN,
            };
            let started = |after| match after {
                State::Starting => false,
                _ => action != Action::Start || outcome.is_err() || after == State::Running,
            };
            while !started(after) && waiting.elapsed() < budget(after) {
                tokio::time::sleep(Duration::from_millis(100)).await;
                after = manager.state(service).unwrap_or(after);
            }
            let installed = after != State::NotInstalled;
            if installed && manager.lingers() == Some(false) {
                let _ = manager.linger();
            }
            Report {
                problem: outcome.err(),
                lingers: installed.then(|| manager.lingers()).flatten(),
                started: before != State::Running && after == State::Running,
                removed: before != State::NotInstalled && !installed,
                ..Report::new(manager, name, after)
            }
        }
    };
    report.folders = Some(folders);
    report.log = service.log(manager.kind());
    if report.state == State::Starting && report.problem.is_none() {
        report.problem = Some(format!(
            "it was still starting after {} seconds; its log: {}",
            ipc::STARTING.as_secs(),
            report.log
        ));
    }
    report
}

pub fn system() -> Box<dyn Manager> {
    let uid = rustix::process::getuid().as_raw();
    match cfg!(target_os = "macos") {
        true => Box::new(Launchd { uid }),
        false => Box::new(Systemd { uid }),
    }
}

pub fn this_service(paths: &Paths) -> Result<Service, String> {
    let home = (std::env::var_os("HOME").map(PathBuf::from))
        .filter(|home| home.is_absolute())
        .ok_or("HOME is not set to an absolute path")?;
    let binary =
        std::env::current_exe().map_err(|error| format!("cannot find the koi binary: {error}"))?;
    let mut service = Service::new(paths, &home, &binary)?;
    if std::env::var_os(LIFELINE).is_some() {
        service.suffix = Some(paths.state_key());
        for name in ["HOME", LIFELINE, INVENTORY] {
            let value = std::env::var_os(name).and_then(|value| value.into_string().ok());
            service.env.push((name, value));
        }
    }
    Ok(service)
}

fn background_off(paths: &Paths) -> Result<bool, String> {
    Config::load(&paths.config_file)
        .map(|config| !config.service.background)
        .map_err(|error| error.to_string())
}

pub async fn reconcile(paths: &Paths, asked: bool) -> Report {
    reconcile_with(system().as_ref(), this_service(paths), paths, asked).await
}

async fn reconcile_with(
    manager: &dyn Manager,
    service: Result<Service, String>,
    paths: &Paths,
    asked: bool,
) -> Report {
    let troubled = |problem| Report::troubled(manager, service.as_ref().ok(), problem);
    let lock = match lock_service(paths, LOCK_WAIT).await {
        Ok(lock) => lock,
        Err(problem) => return troubled(problem),
    };
    if let Ok(service) = &service
        && let Some(newer) = newer_installed(manager, service)
    {
        drop(lock);
        return delegated(manager, service, &newer, asked, DELEGATED).await;
    }
    let status = match daemon_status(paths).await {
        Ok(status) => status,
        Err(problem) => return troubled(problem),
    };
    let daemon = match status.running {
        true => Daemon::Running(
            (status.version.as_deref()).and_then(|version| Version::parse(version).ok()),
        ),
        false => Daemon::Absent,
    };
    let folders = status.folders.len();
    let off = match background_off(paths) {
        Ok(off) => off,
        Err(problem) => {
            return Report {
                folders: Some(folders),
                ..troubled(problem)
            };
        }
    };
    let service = match service {
        Ok(service) => service,
        Err(problem) => {
            return Report {
                folders: Some(folders),
                ..Report::unavailable(manager, problem)
            };
        }
    };
    let mut report = converge(manager, &service, folders, off, asked, &daemon).await;
    drop(lock);
    let stopped = report.state == State::Stopped && report.problem.is_none();
    if folders == 0 || !(report.state == State::Running || stopped) {
        return report;
    }
    if report.state == State::Running && answers(paths, ipc::STARTING).await {
        report.blocked = blocked(paths).await;
        return report;
    }
    if let Some(exit) = manager.crashed(&service) {
        report.state = State::Stopped;
        report.summary_problem = Some(format!("its daemon keeps exiting ({exit})"));
        report.problem = Some(format!(
            "its daemon keeps exiting ({exit}); its log: {}",
            report.log
        ));
    } else if !stopped {
        report.summary_problem = Some(format!(
            "it started, but its daemon did not answer within {} seconds",
            ipc::STARTING.as_secs()
        ));
        report.problem = Some(format!(
            "it started, but its daemon did not answer within {} seconds; its log: {}",
            ipc::STARTING.as_secs(),
            report.log
        ));
    }
    report
}

async fn daemon_status(paths: &Paths) -> Result<Status, String> {
    match ipc::run(paths, Request::Status).await {
        Ok(Response::Status { status }) => Ok(status),
        Ok(_) => Err("the daemon sent no status".into()),
        Err(error) => Err(error.message),
    }
}

async fn lock_service(paths: &Paths, within: Duration) -> Result<Option<Flock>, String> {
    let path = paths.service_lock();
    let deadline = Instant::now() + within;
    loop {
        let flags = rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE;
        let mode = rustix::fs::Mode::from_raw_mode(0o666);
        let file = match koidrive_state_file::create_regular(rustix::fs::CWD, &path, flags, mode) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("{} cannot be opened: {error}", path.display())),
        };
        match Flock::take(file) {
            Ok(Some(lock)) => return Ok(Some(lock)),
            Ok(None) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(None) => {
                return Err(format!(
                    "another koi has held {} for {} seconds",
                    path.display(),
                    within.as_secs()
                ));
            }
            Err(error) => return Err(format!("{} cannot be locked: {error}", path.display())),
        }
    }
}

async fn answers(paths: &Paths, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let answer = tokio::time::timeout(left, koidrive_ipc::ask(paths, &Request::Status)).await;
        if matches!(answer, Ok(Ok(Some(_)))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub async fn inspect(paths: &Paths, daemon_running: bool) -> Report {
    let manager = system();
    let service = match this_service(paths) {
        Ok(service) => service,
        Err(problem) => return Report::unavailable(manager.as_ref(), problem),
    };
    let name = manager.name(&service);
    if background_off(paths).unwrap_or(false) {
        return Report::new(manager.as_ref(), name, State::Off);
    }
    let mut report = match manager.state(&service) {
        Err(problem) => Report::unavailable(manager.as_ref(), problem),
        Ok(state) => Report {
            lingers: (state != State::NotInstalled)
                .then(|| manager.lingers())
                .flatten(),
            ..Report::new(manager.as_ref(), name, state)
        },
    };
    report.log = service.log(manager.kind());
    if daemon_running {
        report.blocked = blocked(paths).await;
    }
    report
}

async fn blocked(paths: &Paths) -> Vec<Blocked> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    match koidrive_ipc::ask(paths, &Request::Access).await {
        Ok(Some(Response::Access { blocked })) => blocked,
        _ => Vec::new(),
    }
}

pub async fn forget(paths: &Paths, deleted: &url::Url) -> Result<Option<Report>, String> {
    let associated = koidrive_driver::Credential::load(&paths.credential())
        .map_err(|error| error.to_string())
        .and_then(|credential| match credential {
            Some(credential) => url::Url::parse(&credential.server)
                .map(|server| server.origin() == deleted.origin())
                .map_err(|_| "the saved device server address is invalid".to_owned()),
            None => Ok(true),
        });
    if associated == Ok(false) {
        return Ok(None);
    }
    let manager = system();
    let service = this_service(paths)?;
    let file = service.file(manager.kind());
    let stop = match manager.kind() {
        ServiceManager::Launchd => format!(
            "launchctl bootout gui/{}/{}",
            rustix::process::getuid().as_raw(),
            service.label()
        ),
        ServiceManager::Systemd => {
            format!("systemctl --user disable --now {}", service.unit_name())
        }
    };
    let manual = format!(
        "Stop this profile's service with {stop}, remove its service file {}, then run koi server delete --interactive again. The files in its folders stay where they are.",
        crate::pairing::shell_word(&file.display().to_string())
    );
    let installed = file.try_exists().map_err(|error| format!("could not check whether this device's background service is installed: {error}. {manual}"))?;
    if !installed && matches!(manager.state(&service), Ok(State::NotInstalled) | Err(_)) {
        return Ok(None);
    }
    associated.map_err(|error| format!("{error}. {manual}"))?;
    let report = converge(manager.as_ref(), &service, 0, false, false, &Daemon::Absent).await;
    if report.problem.is_some() || report.state != State::NotInstalled {
        return Err(format!(
            "could not remove this device's background service: {}. {manual}",
            report.failure_message().trim_end_matches('.')
        ));
    }
    Ok(Some(report))
}

#[cfg(test)]
mod tests;
