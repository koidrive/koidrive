// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use koidrive_driver::{Config, Flock, Paths, Status};
use koidrive_ipc::{Blocked, Request, Response};
use schemars::JsonSchema;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::ipc;

const UNIT: &str = include_str!("../systemd/koidrive.service");

const STARTUP: Duration = Duration::from_secs(10);

/// How long a started service may take to show as running. launchd spawns a job a moment after `launchctl bootstrap`
/// returns, so the first read can still find it not running.
const SPAWN: Duration = Duration::from_secs(5);

const PROBE: Duration = Duration::from_secs(5);

pub const DELEGATED: Duration = Duration::from_secs(7 * 60);

const LOCK_WAIT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    suffix: Option<String>,
    binary: String,
    home: PathBuf,
    state_dir: PathBuf,
    env: Vec<(&'static str, Option<String>)>,
}

impl Service {
    pub fn new(paths: &Paths, home: &Path, binary: &Path) -> Result<Self, String> {
        let text = |path: &Path| match path.to_str() {
            Some(text) if !text.chars().any(char::is_control) => Ok(text.to_owned()),
            _ => Err(format!(
                "{} cannot be written into a service file",
                path.display()
            )),
        };
        let parent = |path: &Path| path.parent().map(Path::to_path_buf).unwrap_or_default();
        let config_home = parent(&parent(&paths.config_file));
        let state_home = parent(&paths.state_dir);
        let runtime = match paths.runtime_dir == paths.state_dir {
            true => None,
            false => Some(text(&parent(&paths.runtime_dir))?),
        };
        let default = paths.state_dir == home.join(".local/state/koidrive")
            && paths.config_file == home.join(".config/koidrive/config.toml");
        let suffix = (!default).then(|| paths.state_key());
        let binary = crate::update::homebrew_link(binary).unwrap_or_else(|| binary.to_path_buf());
        Ok(Service {
            suffix,
            binary: text(&binary)?,
            home: home.to_path_buf(),
            state_dir: paths.state_dir.clone(),
            env: vec![
                ("XDG_CONFIG_HOME", Some(text(&config_home)?)),
                ("XDG_STATE_HOME", Some(text(&state_home)?)),
                ("XDG_RUNTIME_DIR", runtime),
            ],
        })
    }

    pub fn unit_name(&self) -> String {
        match &self.suffix {
            None => "koidrive.service".to_owned(),
            Some(suffix) => format!("koidrive-{suffix}.service"),
        }
    }

    pub fn label(&self) -> String {
        match &self.suffix {
            None => "dev.koidrive.koi".to_owned(),
            Some(suffix) => format!("dev.koidrive.koi.{suffix}"),
        }
    }

    fn unit_file(&self) -> PathBuf {
        self.state_dir.join("systemd").join(self.unit_name())
    }

    fn plist_file(&self) -> PathBuf {
        (self.home.join("Library/LaunchAgents")).join(format!("{}.plist", self.label()))
    }

    pub fn log_file(&self) -> PathBuf {
        (self.home.join("Library/Logs/Koidrive")).join(format!("{}.log", self.label()))
    }

    pub fn unit(&self) -> String {
        let mut unit = String::new();
        for line in UNIT.lines() {
            match line.strip_prefix("ExecStart=") {
                Some(_) => unit.push_str(&format!(
                    "ExecStart={} _daemon\n",
                    systemd_quoted(&self.binary)
                )),
                None => unit.push_str(&format!("{line}\n")),
            }
            if line == "[Service]" {
                for (name, value) in &self.env {
                    unit.push_str(&match value {
                        Some(value) => format!(
                            "Environment={}\n",
                            systemd_quoted(&format!("{name}={value}"))
                        ),
                        None => format!("UnsetEnvironment={name}\n"),
                    });
                }
            }
        }
        unit
    }

    /// The launch agent's property list. A plist cannot unset a variable that launchd's own environment sets, so an
    /// unset one is written empty, which the daemon reads as unset.
    pub fn plist(&self) -> String {
        let mut env = String::from("\t\t<key>KOIDRIVE_LOG</key>\n\t\t<string>info</string>\n");
        for (name, value) in &self.env {
            env.push_str(&format!(
                "\t\t<key>{name}</key>\n\t\t<string>{}</string>\n",
                xml(value.as_deref().unwrap_or_default())
            ));
        }
        let log = xml(&self.log_file().display().to_string());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{binary}</string>
		<string>_daemon</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
{env}	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
            label = self.label(),
            binary = xml(&self.binary),
        )
    }

    fn log(&self, manager: ServiceManager) -> String {
        match manager {
            ServiceManager::Launchd => self.log_file().display().to_string(),
            _ => format!("journalctl --user -u {}", self.unit_name()),
        }
    }
}

/// A quoted word of a unit file: `%` starts a specifier, and quotes and backslashes need escapes.
fn systemd_quoted(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

fn unit_binary(unit: &str) -> Option<String> {
    let exec = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))?;
    let quoted = exec
        .strip_suffix(" _daemon")?
        .strip_prefix('"')?
        .strip_suffix('"')?;
    let mut binary = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' | '%' => binary.push(chars.next()?),
            c => binary.push(c),
        }
    }
    Some(binary)
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn plist_binary(plist: &str) -> Option<String> {
    let (_, arguments) = plist.split_once("<key>ProgramArguments</key>")?;
    let (_, binary) = arguments.split_once("<string>")?;
    let (binary, _) = binary.split_once("</string>")?;
    Some(
        binary
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

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

/// The service as a command left it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Report {
    pub manager: ServiceManager,
    pub name: String,
    pub state: State,
    /// Why the service cannot run, or did not change as it should.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lingers: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked: Vec<Blocked>,
    #[serde(skip)]
    started: bool,
    #[serde(skip)]
    removed: bool,
    /// The folders the profile links, as the command that brought the service in line counted them. Left out when it
    /// links none or the command did not count them.
    #[serde(default, skip_serializing_if = "no_folders")]
    #[schemars(with = "usize")]
    folders: Option<usize>,
    #[serde(skip)]
    log: String,
}

fn no_folders(folders: &Option<usize>) -> bool {
    folders.is_none_or(|count| count == 0)
}

impl Report {
    fn new(manager: &dyn Manager, name: String, state: State) -> Self {
        Report {
            manager: manager.kind(),
            name,
            state,
            problem: None,
            lingers: None,
            blocked: Vec::new(),
            started: false,
            removed: false,
            folders: None,
            log: String::new(),
        }
    }

    fn unavailable(manager: &dyn Manager, problem: String) -> Self {
        Report {
            problem: Some(problem),
            ..Report::new(manager, String::new(), State::Unavailable)
        }
    }

    fn troubled(manager: &dyn Manager, service: Option<&Service>, problem: String) -> Self {
        let Some(service) = service else {
            return Report::unavailable(manager, problem);
        };
        let state = manager.state(service).unwrap_or(State::Unavailable);
        Report {
            problem: Some(problem),
            log: service.log(manager.kind()),
            ..Report::new(manager, manager.name(service), state)
        }
    }

    pub fn retryable(&self) -> bool {
        self.state == State::Running && self.problem.is_some()
    }

    pub fn text(&self) -> String {
        let mut text = String::new();
        let launchd = self.manager == ServiceManager::Launchd;
        let folders = self.folders.unwrap_or(0);
        let problem = (self.problem.as_deref()).map(crate::printable);
        match self.state {
            State::Off if folders > 0 => text.push_str(
                "Background sync is off in config.toml, so folders sync only while koi _daemon runs.\n",
            ),
            State::Unavailable if folders > 0 || self.folders.is_none() && self.problem.is_some() => {
                text.push_str(&format!(
                    "Koidrive can't sync in the background: {}.\n",
                    problem.as_deref().unwrap_or("no service manager")
                ));
                text.push_str(match launchd {
                    true => "Log in to this Mac's desktop, then run: koi service start\n",
                    false => "Log in as this user directly rather than through su or sudo, or run sudo loginctl enable-linger \"$USER\" and log in again. Then run: koi service start\nWithout systemd, keep koi _daemon running with your own service manager.\n",
                });
            }
            State::Running if self.started => {
                text.push_str(&format!("Syncing in the background ({}).\n", self.name));
                text.push_str(match (launchd, self.lingers) {
                    (true, _) => "It starts again when you log in. If macOS shows \"Background Items Added\", leave koi switched on in System Settings > General > Login Items.\n",
                    (false, Some(false)) => "It stops when you log out and starts at your next login. To keep it running without a login, run: sudo loginctl enable-linger \"$USER\"\n",
                    (false, Some(true)) => "It keeps running after you log out and starts again after a restart.\n",
                    (false, None) => "",
                });
            }
            State::Stopped if folders > 0 => text.push_str(&format!(
                "The background service {} is installed but not running. Its log: {}\nTo start it again, run: koi service start\n",
                self.name, self.log
            )),
            State::SwitchedOff if folders > 0 => text.push_str(SWITCHED_OFF),
            State::NotInstalled if self.removed => {
                text.push_str("Stopped and removed the background service.\n")
            }
            _ => {}
        }
        if let Some(problem) = problem.filter(|_| self.state != State::Unavailable) {
            text.push_str(&format!("The background service: {problem}\n"));
        }
        text.push_str(&blocked_text(&self.blocked));
        text
    }

    pub fn failed(&self) -> bool {
        self.problem.is_some()
            || matches!(
                self.state,
                State::Stopped | State::SwitchedOff | State::Unavailable
            )
    }

    pub fn failure_message(&self) -> String {
        if self.state == State::SwitchedOff && self.problem.is_none() {
            return SWITCHED_OFF.trim_end().to_owned();
        }
        self.problem
            .clone()
            .unwrap_or_else(|| self.status_line())
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn status_line(&self) -> String {
        let state = match self.state {
            State::Running => format!("running ({})", self.name),
            State::Starting => format!("starting ({})", self.name),
            State::Stopped => format!("installed, not running ({}); log: {}", self.name, self.log),
            State::SwitchedOff => format!("switched off in Login Items ({})", self.name),
            State::NotInstalled => "not installed; it starts when you link a folder".to_owned(),
            State::Unavailable => format!(
                "unavailable: {}",
                crate::printable(self.problem.as_deref().unwrap_or_default())
            ),
            State::Off => "off in config.toml".to_owned(),
        };
        let mut line = format!("Background service: {state}\n");
        if self.state == State::SwitchedOff {
            line.push_str(&format!("  {SWITCHED_OFF}"));
        }
        if self.lingers == Some(false) {
            line.push_str("  It stops when you log out. To keep it running, run: sudo loginctl enable-linger \"$USER\"\n");
        }
        line.push_str(&blocked_text(&self.blocked));
        line
    }
}

const SWITCHED_OFF: &str = "koi is switched off in System Settings > General > Login Items > Allow in the Background, so the folders don't sync. To sync them in the background, switch it on, then run: koi service start\n";

fn blocked_text(blocked: &[Blocked]) -> String {
    let mut text = String::new();
    for blocked in blocked {
        let folder = crate::printable(&blocked.folder);
        let word = crate::pairing::shell_word(&folder);
        text.push_str(&match blocked.waiting {
            true => format!("koi is still waiting to read {folder}. If macOS asks whether koi may access it, click Allow, so that it syncs in the background.\n"),
            false => format!("macOS doesn't let koi read {folder} in the background. Open System Settings > Privacy & Security > Files & Folders and switch the folder on under koi, or add koi under Full Disk Access. Then run: koi folder restart {word}\n"),
        });
    }
    text
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Daemon {
    Absent,
    Running(Option<Version>),
}

#[derive(Debug, PartialEq, Eq)]
struct Target {
    keep: bool,
    restart: bool,
}

fn target(installed: Option<&Version>, caller: &Version, daemon: &Daemon) -> Target {
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
    let mut command = Command::new(binary);
    command.arg("--version").stderr(Stdio::null());
    let (status, printed) = output_within(&mut command, PROBE).ok()??;
    if !status.success() {
        return None;
    }
    let printed = String::from_utf8(printed).ok()?;
    Version::parse(printed.strip_prefix("koi ")?.trim()).ok()
}

fn volatile_binary(binary: &Path) -> bool {
    let volatile = |path: &Path| {
        path.starts_with("/Volumes")
            || path
                .components()
                .any(|part| part.as_os_str() == "AppTranslocation")
    };
    volatile(binary) || fs::canonicalize(binary).is_ok_and(|path| volatile(&path))
}

const MOVE_TO_APPLICATIONS: &str =
    "Move Koidrive to Applications, then open it again to enable background sync.";

fn newer_installed(manager: &dyn Manager, service: &Service) -> Option<PathBuf> {
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

async fn delegated(
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
    let mut command = tokio::process::Command::new(binary);
    command.args(args).stderr(Stdio::inherit());
    let output = delegated_output(&mut command, within).await;
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

fn start(manager: &dyn Manager, service: &Service, daemon: &Daemon) -> Result<(), String> {
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
        Ok(before) => {
            let action = plan(folders, before, asked);
            let daemon = match before {
                State::Running => daemon,
                _ => &Daemon::Absent,
            };
            let outcome = match action {
                Action::Leave => Ok(()),
                Action::Start => start(manager, service, daemon),
                Action::Remove => manager.remove(service),
            };
            let mut after = manager.state(service).unwrap_or(before);
            let spawned = Instant::now()
                + match after {
                    State::Starting => STARTUP,
                    _ => SPAWN,
                };
            let started = |after| match after {
                State::Starting => false,
                _ => action != Action::Start || outcome.is_err() || after == State::Running,
            };
            while !started(after) && Instant::now() < spawned {
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
    Service::new(paths, &home, &binary)
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
    if report.state != State::Running || folders == 0 {
        return report;
    }
    if !answers(paths, STARTUP).await {
        if let Some(exit) = manager.crashed(&service) {
            report.state = State::Stopped;
            report.problem = Some(format!(
                "its daemon keeps exiting ({exit}); its log: {}",
                report.log
            ));
            return report;
        }
        report.problem = Some(format!(
            "it started, but its daemon did not answer within {} seconds; its log: {}",
            STARTUP.as_secs(),
            report.log
        ));
        return report;
    }
    report.blocked = blocked(paths).await;
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
        let file = match fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        {
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

pub async fn forget(paths: &Paths, deleted: &url::Url) -> Option<Report> {
    let credential = koidrive_driver::Credential::load(&paths.credential()).ok()??;
    let server = url::Url::parse(&credential.server).ok()?;
    if server.origin() != deleted.origin() {
        return None;
    }
    let manager = system();
    let service = this_service(paths).ok()?;
    Some(
        converge(
            manager.as_ref(),
            &service,
            0,
            background_off(paths).unwrap_or(false),
            false,
            &Daemon::Absent,
        )
        .await,
    )
}

fn write(path: &Path, text: &str) -> Result<bool, String> {
    if fs::read(path).is_ok_and(|old| old == text.as_bytes()) {
        return Ok(false);
    }
    let failed = |error: io::Error| format!("cannot write {}: {error}", path.display());
    let dir = path.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(dir).map_err(failed)?;
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(failed)?;
    file.write_all(text.as_bytes()).map_err(failed)?;
    file.persist(path).map_err(|error| failed(error.error))?;
    Ok(true)
}

fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(format!("cannot remove {}: {error}", path.display()))
        }
        _ => Ok(()),
    }
}

fn run(program: &str, args: &[&str]) -> Result<Output, String> {
    Command::new(program)
        .args(args)
        .output()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => format!("{program} is not installed"),
            _ => format!("cannot run {program}: {error}"),
        })
}

fn ok(program: &str, args: &[&str]) -> Result<String, String> {
    let output = run(program, args)?;
    match output.status.success() {
        true => Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned()),
        false => Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

pub struct Systemd {
    uid: u32,
}

fn systemctl(args: &[&str]) -> Result<String, String> {
    let args: Vec<&str> = ["--user"].iter().chain(args).copied().collect();
    ok("systemctl", &args)
}

impl Manager for Systemd {
    fn kind(&self) -> ServiceManager {
        ServiceManager::Systemd
    }

    fn name(&self, service: &Service) -> String {
        service.unit_name()
    }

    fn state(&self, service: &Service) -> Result<State, String> {
        systemctl(&["show-environment"]).map_err(|error| {
            format!("there is no systemd user session to start it in ({error})")
        })?;
        let active = run(
            "systemctl",
            &["--user", "is-active", "--quiet", &service.unit_name()],
        )?;
        Ok(match active.status.success() {
            true => State::Running,
            false if service.unit_file().exists() => State::Stopped,
            false => State::NotInstalled,
        })
    }

    fn installed(&self, service: &Service) -> Option<String> {
        unit_binary(&fs::read_to_string(service.unit_file()).ok()?)
    }

    fn start(&self, service: &Service, restart: bool) -> Result<(), String> {
        let file = service.unit_file();
        let changed = write(&file, &service.unit())?;
        systemctl(&["enable", &file.display().to_string()])?;
        systemctl(&["daemon-reload"])?;
        let verb = if changed || restart {
            "restart"
        } else {
            "start"
        };
        systemctl(&[verb, &service.unit_name()]).map(drop)
    }

    fn remove(&self, service: &Service) -> Result<(), String> {
        let name = service.unit_name();
        if run("systemctl", &["--user", "cat", &name])?
            .status
            .success()
        {
            systemctl(&["disable", "--now", &name])?;
        }
        remove_file(&service.unit_file())?;
        systemctl(&["daemon-reload"]).map(drop)
    }

    fn lingers(&self) -> Option<bool> {
        let uid = self.uid.to_string();
        let linger = ok(
            "loginctl",
            &["show-user", &uid, "--property=Linger", "--value"],
        )
        .ok()?;
        match linger.as_str() {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        }
    }

    /// Turns lingering on where polkit allows it without a password, as it does by default in a local session.
    fn linger(&self) -> Result<(), String> {
        ok("loginctl", &["--no-ask-password", "enable-linger"]).map(drop)
    }
}

pub struct Launchd {
    uid: u32,
}

impl Launchd {
    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn target(&self, service: &Service) -> String {
        format!("{}/{}", self.domain(), service.label())
    }

    fn disabled(&self, service: &Service) -> Result<bool, String> {
        let disabled = ok("launchctl", &["print-disabled", &self.domain()])?;
        Ok(launchd_disabled(&disabled, &service.label()))
    }

    fn loaded(&self, service: &Service) -> Result<Option<String>, String> {
        let output = run("launchctl", &["print", &self.target(service)])?;
        Ok((output.status.success()).then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
    }
}

impl Manager for Launchd {
    fn kind(&self) -> ServiceManager {
        ServiceManager::Launchd
    }

    fn name(&self, service: &Service) -> String {
        service.label()
    }

    fn state(&self, service: &Service) -> Result<State, String> {
        ok("launchctl", &["print", &self.domain()]).map_err(|_| {
            "this user is not logged in to the Mac's desktop, so launchd has no session to start it in"
                .to_owned()
        })?;
        Ok(match self.loaded(service)? {
            Some(job) => launchd_state(&job),
            None => unloaded(service.plist_file().exists(), self.disabled(service)?),
        })
    }

    fn installed(&self, service: &Service) -> Option<String> {
        plist_binary(&fs::read_to_string(service.plist_file()).ok()?)
    }

    fn crashed(&self, service: &Service) -> Option<String> {
        launchd_exit(&self.loaded(service).ok()??)
    }

    fn start(&self, service: &Service, restart: bool) -> Result<(), String> {
        if self.disabled(service)? {
            return Err(SWITCHED_OFF.trim_end().to_owned());
        }
        let log = service.log_file();
        let dir = log.parent().unwrap_or(Path::new("/"));
        fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let plist = service.plist_file();
        let existed = plist.exists();
        let changed = write(&plist, &service.plist())?;
        let target = self.target(service);
        let loaded = self.loaded(service)?.is_some();
        if loaded && !changed {
            let args: &[&str] = match restart {
                true => &["kickstart", "-k", &target],
                false => &["kickstart", &target],
            };
            return ok("launchctl", args).map(drop);
        }
        if loaded {
            ok("launchctl", &["bootout", &target])?;
        }
        let path = plist.display().to_string();
        let mut attempts = 0;
        loop {
            match ok("launchctl", &["bootstrap", &self.domain(), &path]) {
                Ok(_) => return Ok(()),
                Err(_) if attempts < 3 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(error) => {
                    if !existed {
                        remove_file(&plist)?;
                    }
                    return Err(format!(
                        "{error}. If koi is switched off in System Settings > General > Login Items > Allow in the Background, switch it on"
                    ));
                }
            }
        }
    }

    fn remove(&self, service: &Service) -> Result<(), String> {
        if self.loaded(service)?.is_some() {
            ok("launchctl", &["bootout", &self.target(service)])?;
        }
        remove_file(&service.plist_file())
    }
}

fn unloaded(plist: bool, disabled: bool) -> State {
    match (plist, disabled) {
        (false, _) => State::NotInstalled,
        (true, true) => State::SwitchedOff,
        (true, false) => State::Stopped,
    }
}

fn launchd_field<'a>(job: &'a str, name: &str) -> Option<&'a str> {
    job.lines()
        .find_map(|line| line.trim().strip_prefix(name)?.strip_prefix(" = "))
}

fn launchd_state(job: &str) -> State {
    match (launchd_field(job, "state"), launchd_exit(job)) {
        (Some("running"), _) => State::Running,
        (Some("SIGTERMed" | "spawn scheduled"), None) => State::Starting,
        _ => State::Stopped,
    }
}

fn launchd_exit(job: &str) -> Option<String> {
    match launchd_field(job, "last exit code")?.parse::<i32>() {
        Ok(0) | Err(_) => None,
        Ok(code) => Some(format!("exit code {code}")),
    }
}

fn launchd_disabled(disabled: &str, label: &str) -> bool {
    let entry = format!("\"{label}\" => ");
    disabled
        .lines()
        .any(|line| matches!(line.trim().strip_prefix(&entry), Some("disabled" | "true")))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    #[test]
    fn a_disabled_launch_agent_is_distinct_from_an_unloaded_one() {
        let disabled = r#"disabled services = {
            "dev.koidrive.koi.other" => disabled
            "dev.koidrive.koi.enabled" => enabled
            "dev.koidrive.koi.disabled" => disabled
        }"#;
        assert!(launchd_disabled(disabled, "dev.koidrive.koi.disabled"));
        assert!(!launchd_disabled(disabled, "dev.koidrive.koi.enabled"));
        assert!(!launchd_disabled(disabled, "dev.koidrive.koi.absent"));
        assert!(launchd_disabled(
            "\"dev.koidrive.koi.disabled\" => true",
            "dev.koidrive.koi.disabled"
        ));
    }

    #[test]
    fn only_a_disabled_agent_is_switched_off_and_an_unloaded_one_starts_again() {
        assert_eq!(unloaded(true, true), State::SwitchedOff);
        assert_eq!(unloaded(true, false), State::Stopped);
        assert_eq!(unloaded(false, false), State::NotInstalled);
        assert_eq!(unloaded(false, true), State::NotInstalled);
        assert_eq!(plan(1, unloaded(true, false), false), Action::Start);
        assert_eq!(plan(1, unloaded(true, true), false), Action::Leave);
    }

    #[test]
    fn a_failed_start_of_a_disabled_agent_has_one_login_items_message() {
        let mut report = Report::new(&Fake::default(), "koi".into(), State::SwitchedOff);
        assert_eq!(report.failure_message(), SWITCHED_OFF.trim_end());
        report.problem = Some(format!("bootstrap failed. {SWITCHED_OFF}"));
        report.folders = Some(1);
        let message = report.failure_message();
        assert_eq!(
            message,
            format!("bootstrap failed. {}", SWITCHED_OFF.trim_end())
        );
        assert!(!message.contains('\n'));
        assert!(message.contains("bootstrap"));
        assert!(!crate::printable(&message).contains("\\u{a}"));
    }

    #[test]
    fn a_failed_status_probe_is_reported_without_a_folder_count() {
        let report = Report::unavailable(
            &Fake::default(),
            "the daemon did not answer within 5 seconds".into(),
        );
        assert_eq!(report.folders, None);
        let text = report.text();
        assert!(
            text.contains("the daemon did not answer within 5 seconds"),
            "{text}"
        );
        assert!(text.contains("can't sync in the background"), "{text}");
        let unavailable = Report::new(&Fake::default(), String::new(), State::Unavailable);
        assert!(unavailable.text().is_empty());
    }

    fn paths(home: &str, vars: &[(&str, &str)]) -> Paths {
        Paths::from_vars(|name| {
            (vars.iter().chain(&[("HOME", home)]))
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.into())
        })
        .unwrap()
    }

    fn service(vars: &[(&str, &str)]) -> Service {
        let home = "/home/ada";
        let binary = Path::new("/home/ada/.local/bin/koi");
        Service::new(&paths(home, vars), Path::new(home), binary).unwrap()
    }

    #[test]
    fn the_default_profile_has_the_plain_names_and_any_other_one_names_of_its_own() {
        let default = service(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
        assert_eq!(default.unit_name(), "koidrive.service");
        assert_eq!(default.label(), "dev.koidrive.koi");
        let other = service(&[("XDG_STATE_HOME", "/srv/ada/state")]);
        let third = service(&[("XDG_STATE_HOME", "/srv/bob/state")]);
        assert!(
            other.unit_name().starts_with("koidrive-"),
            "{}",
            other.unit_name()
        );
        assert!(
            other.label().starts_with("dev.koidrive.koi."),
            "{}",
            other.label()
        );
        assert_ne!(other.unit_name(), third.unit_name());
        assert_eq!(other, service(&[("XDG_STATE_HOME", "/srv/ada/state")]));
    }

    #[test]
    fn the_unit_runs_this_binary_with_the_profiles_directories() {
        let unit = service(&[("XDG_RUNTIME_DIR", "/run/user/1000")]).unit();
        let lines: Vec<&str> = unit.lines().filter(|line| !line.starts_with('#')).collect();
        for expected in [
            "ExecStart=\"/home/ada/.local/bin/koi\" _daemon",
            "Environment=\"XDG_CONFIG_HOME=/home/ada/.config\"",
            "Environment=\"XDG_STATE_HOME=/home/ada/.local/state\"",
            "Environment=\"XDG_RUNTIME_DIR=/run/user/1000\"",
            "Restart=on-failure",
            "WantedBy=default.target",
        ] {
            assert!(lines.contains(&expected), "{expected} in {unit}");
        }
        let section = lines.iter().position(|line| *line == "[Service]").unwrap();
        let environment = (lines.iter())
            .position(|line| line.starts_with("Environment=\"XDG"))
            .unwrap();
        assert!(environment > section, "{unit}");
    }

    #[test]
    fn a_profile_without_a_runtime_directory_keeps_systemds_out_of_the_daemon() {
        let unit = service(&[]).unit();
        assert!(
            unit.lines()
                .any(|line| line == "UnsetEnvironment=XDG_RUNTIME_DIR"),
            "{unit}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_launch_agent_opens_this_profile_even_when_launchd_sets_a_runtime_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.plist");
        fs::write(&file, service(&[]).plist()).unwrap();
        let json = Command::new("plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(&file)
            .output()
            .unwrap();
        assert!(json.status.success());
        let plist: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        let mut env = std::collections::BTreeMap::from([
            ("HOME".to_owned(), "/home/ada".to_owned()),
            (
                "XDG_RUNTIME_DIR".to_owned(),
                "/inherited/runtime".to_owned(),
            ),
        ]);
        for (name, value) in plist["EnvironmentVariables"].as_object().unwrap() {
            env.insert(name.clone(), value.as_str().unwrap().to_owned());
        }
        let opened = Paths::from_vars(|name| env.get(name).map(Into::into)).unwrap();
        assert_eq!(opened, paths("/home/ada", &[]));
    }

    #[tokio::test]
    async fn a_daemon_that_takes_the_connection_and_never_answers_does_not_hold_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let _held = std::os::unix::net::UnixListener::bind(paths.socket()).unwrap();
        let answered = tokio::time::timeout(
            Duration::from_secs(5),
            answers(&paths, Duration::from_millis(300)),
        )
        .await;
        assert_eq!(answered, Ok(false));
    }

    #[test]
    fn quotes_specifiers_and_dollars_reach_the_daemon_as_written() {
        let home = "/home/ada";
        let paths = paths(home, &[("XDG_STATE_HOME", "/data/100% \"sure\"")]);
        let service = Service::new(&paths, Path::new(home), Path::new("/opt/my $bin/koi")).unwrap();
        let unit = service.unit();
        assert!(
            unit.contains("ExecStart=\"/opt/my $bin/koi\" _daemon\n"),
            "{unit}"
        );
        assert!(
            unit.contains("Environment=\"XDG_STATE_HOME=/data/100%% \\\"sure\\\"\"\n"),
            "{unit}"
        );
        assert!(
            service
                .plist()
                .contains("<string>/opt/my $bin/koi</string>")
        );
    }

    #[test]
    fn paths_a_service_file_cannot_carry_are_refused() {
        let home = "/home/ada";
        let binary = Path::new("/opt/koi\n[Service]");
        let refused = Service::new(&paths(home, &[]), Path::new(home), binary).unwrap_err();
        assert!(refused.contains("cannot be written"), "{refused}");
    }

    #[test]
    fn the_launch_agent_starts_at_login_stays_alive_and_logs_to_the_library() {
        let plist = service(&[("XDG_STATE_HOME", "/Users/ada/a&b")]).plist();
        for expected in [
            "<key>RunAtLoad</key>\n\t<true/>",
            "<key>KeepAlive</key>\n\t<true/>",
            "<string>/home/ada/.local/bin/koi</string>\n\t\t<string>_daemon</string>",
            "<key>XDG_STATE_HOME</key>\n\t\t<string>/Users/ada/a&amp;b</string>",
            "<string>/home/ada/Library/Logs/Koidrive/dev.koidrive.koi.",
        ] {
            assert!(plist.contains(expected), "{expected} in {plist}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plutil_accepts_the_launch_agent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.plist");
        fs::write(
            &file,
            service(&[("XDG_STATE_HOME", "/Users/a <b>")]).plist(),
        )
        .unwrap();
        let lint = Command::new("plutil")
            .arg("-lint")
            .arg(&file)
            .output()
            .unwrap();
        assert!(
            lint.status.success(),
            "{}",
            String::from_utf8_lossy(&lint.stdout)
        );
    }

    #[test]
    fn the_service_runs_while_a_folder_is_linked_and_goes_with_the_last_one() {
        use State::*;
        assert_eq!(plan(1, NotInstalled, false), Action::Start);
        assert_eq!(plan(2, Stopped, false), Action::Start);
        assert_eq!(plan(1, Running, false), Action::Start);
        assert_eq!(plan(0, Running, false), Action::Remove);
        assert_eq!(plan(0, Stopped, false), Action::Remove);
        assert_eq!(plan(0, NotInstalled, false), Action::Leave);
        assert_eq!(plan(1, Unavailable, false), Action::Leave);
        assert_eq!(plan(0, Off, false), Action::Leave);
    }

    #[test]
    fn an_agent_switched_off_in_login_items_starts_only_when_asked_and_goes_with_the_last_folder() {
        use State::*;
        assert_eq!(plan(1, SwitchedOff, false), Action::Leave);
        assert_eq!(plan(1, SwitchedOff, true), Action::Start);
        assert_eq!(plan(0, SwitchedOff, false), Action::Remove);
    }

    #[derive(Default)]
    struct Fake {
        unavailable: bool,
        unit: RefCell<Option<String>>,
        running: Cell<bool>,
        restarts: Cell<u32>,
        lingers: Cell<Option<bool>>,
        linger_refused: bool,
        spawning: Cell<u32>,
        switched_off: Cell<bool>,
        remove_problem: Option<&'static str>,
        service_lock: Option<PathBuf>,
        locked_at_start: Cell<Option<bool>>,
        crashed: Option<&'static str>,
    }

    impl Manager for Fake {
        fn kind(&self) -> ServiceManager {
            ServiceManager::Systemd
        }

        fn name(&self, service: &Service) -> String {
            service.unit_name()
        }

        fn state(&self, _: &Service) -> Result<State, String> {
            if self.unavailable {
                return Err("no user bus".to_owned());
            }
            let spawning = self.spawning.get();
            self.spawning.set(spawning.saturating_sub(1));
            Ok(match (self.unit.borrow().is_some(), self.running.get()) {
                (true, _) if self.switched_off.get() => State::SwitchedOff,
                (true, true) if spawning > 0 => State::Stopped,
                (_, true) => State::Running,
                (true, false) => State::Stopped,
                (false, false) => State::NotInstalled,
            })
        }

        fn crashed(&self, _: &Service) -> Option<String> {
            self.crashed.map(str::to_owned)
        }

        fn installed(&self, _: &Service) -> Option<String> {
            unit_binary(self.unit.borrow().as_deref()?)
        }

        fn start(&self, service: &Service, restart: bool) -> Result<(), String> {
            if let Some(path) = &self.service_lock {
                let file = (fs::File::options().create(true).truncate(false).write(true))
                    .open(path)
                    .unwrap();
                let taken =
                    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive);
                self.locked_at_start.set(Some(taken.is_err()));
            }
            let unit = Some(service.unit());
            if (*self.unit.borrow() != unit || restart) && self.running.get() {
                self.restarts.set(self.restarts.get() + 1);
            }
            *self.unit.borrow_mut() = unit;
            self.running.set(true);
            self.switched_off.set(false);
            Ok(())
        }

        fn remove(&self, _: &Service) -> Result<(), String> {
            if let Some(problem) = self.remove_problem {
                return Err(problem.into());
            }
            *self.unit.borrow_mut() = None;
            self.running.set(false);
            Ok(())
        }

        fn lingers(&self) -> Option<bool> {
            self.lingers.get()
        }

        fn linger(&self) -> Result<(), String> {
            if self.linger_refused {
                return Err("interactive authentication required".to_owned());
            }
            self.lingers.set(Some(true));
            Ok(())
        }
    }

    #[tokio::test]
    async fn linking_starts_the_service_once_and_unlinking_the_last_folder_removes_it() {
        let fake = Fake::default();
        let service = service(&[]);
        let first = converge(&fake, &service, 1, false, false, &Daemon::Absent).await;
        assert_eq!((first.state, first.started), (State::Running, true));
        let text = first.text();
        assert!(
            text.contains("Syncing in the background (koidrive.service)"),
            "{text}"
        );
        assert!(!text.contains("log out"), "lingering is unknown: {text}");
        let second = converge(&fake, &service, 2, false, false, &Daemon::Absent).await;
        assert_eq!((second.state, second.started), (State::Running, false));
        assert_eq!(second.text(), "");
        assert_eq!(fake.restarts.get(), 0);
        assert_eq!(
            converge(&fake, &service, 1, false, false, &Daemon::Absent)
                .await
                .state,
            State::Running
        );
        let removed = converge(&fake, &service, 0, false, false, &Daemon::Absent).await;
        assert_eq!(
            (removed.state, removed.removed),
            (State::NotInstalled, true)
        );
        assert_eq!(
            removed.text(),
            "Stopped and removed the background service.\n"
        );
        assert_eq!(*fake.unit.borrow(), None);
        let again = converge(&fake, &service, 0, false, false, &Daemon::Absent).await;
        assert_eq!((again.state, again.removed), (State::NotInstalled, false));
        assert_eq!(again.text(), "");
    }

    #[tokio::test]
    async fn a_service_that_spawns_a_moment_after_it_starts_reads_as_running() {
        let fake = Fake {
            spawning: Cell::new(3),
            ..Fake::default()
        };
        let report = converge(&fake, &service(&[]), 1, false, false, &Daemon::Absent).await;
        assert_eq!((report.state, report.started), (State::Running, true));
        assert!(!report.text().contains("not running"), "{}", report.text());
    }

    #[tokio::test]
    async fn a_service_spawn_wait_can_be_cancelled_without_waiting_for_its_deadline() {
        let fake = Fake {
            spawning: Cell::new(u32::MAX),
            ..Fake::default()
        };
        let service = service(&[]);
        let started = Instant::now();
        let result = tokio::time::timeout(Duration::from_millis(250), async {
            converge(&fake, &service, 1, false, false, &Daemon::Absent).await
        })
        .await;
        assert!(
            result.is_err(),
            "the service must still be waiting to spawn"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(fake.unit.borrow().is_some());
    }

    #[tokio::test]
    async fn a_link_leaves_an_agent_switched_off_in_login_items_off_and_says_how_to_switch_it_on() {
        let fake = Fake::default();
        let service = service(&[]);
        converge(&fake, &service, 1, false, false, &Daemon::Absent).await;
        fake.running.set(false);
        fake.switched_off.set(true);
        let linked = converge(&fake, &service, 2, false, false, &Daemon::Absent).await;
        assert_eq!(linked.state, State::SwitchedOff);
        assert!(!fake.running.get());
        let text = linked.text();
        assert!(
            text.contains("Login Items > Allow in the Background"),
            "{text}"
        );
        assert!(
            text.contains("switch it on, then run: koi service start"),
            "{text}"
        );
        let line = linked.status_line();
        assert!(
            line.contains("switched off in Login Items (koidrive.service)"),
            "{line}"
        );
        assert!(linked.failed());
        let asked = converge(&fake, &service, 2, false, true, &Daemon::Absent).await;
        assert_eq!((asked.state, asked.started), (State::Running, true));
        assert!(!asked.failed());
    }

    #[tokio::test]
    async fn removing_a_disabled_agent_preserves_the_removal_failure() {
        let service = service(&[]);
        let fake = Fake {
            unit: RefCell::new(Some(service.unit())),
            switched_off: Cell::new(true),
            remove_problem: Some("removal refused"),
            ..Fake::default()
        };
        let report = converge(&fake, &service, 0, false, false, &Daemon::Absent).await;
        assert_eq!(report.state, State::SwitchedOff);
        assert!(report.failed());
        assert_eq!(report.problem.as_deref(), Some("removal refused"));
        assert_eq!(report.failure_message(), "removal refused");
        assert!(!report.removed);
        assert_eq!(*fake.unit.borrow(), Some(service.unit()));
    }

    #[tokio::test]
    async fn a_moved_binary_restarts_the_running_service_with_the_new_unit() {
        let fake = Fake::default();
        converge(&fake, &service(&[]), 1, false, false, &Daemon::Absent).await;
        let home = "/home/ada";
        let binary = Path::new("/usr/local/bin/koi");
        let moved = Service::new(&paths(home, &[]), Path::new(home), binary).unwrap();
        converge(&fake, &moved, 1, false, false, &Daemon::Absent).await;
        assert_eq!(fake.restarts.get(), 1);
        assert!(
            fake.unit
                .borrow()
                .as_ref()
                .unwrap()
                .contains("/usr/local/bin/koi")
        );
    }

    #[test]
    fn a_homebrew_koi_runs_the_service_from_the_link_that_outlives_brew_upgrade() {
        let cases = [
            ("/Users/ada", "/opt/homebrew"),
            ("/Users/ada", "/usr/local"),
            ("/home/ada", "/home/linuxbrew/.linuxbrew"),
        ];
        for (home, prefix) in cases {
            let keg = format!("{prefix}/Cellar/koidrive/0.1.0/bin/koi");
            let service =
                Service::new(&paths(home, &[]), Path::new(home), Path::new(&keg)).unwrap();
            let link = format!("{prefix}/opt/koidrive/bin/koi");
            assert!(
                service
                    .unit()
                    .contains(&format!("ExecStart=\"{link}\" _daemon\n")),
                "{}",
                service.unit()
            );
            assert!(service.plist().contains(&format!(
                "<string>{link}</string>\n\t\t<string>_daemon</string>"
            )));
            assert_eq!(unit_binary(&service.unit()).as_deref(), Some(link.as_str()));
        }
        let own = Service::new(
            &paths("/home/ada", &[]),
            Path::new("/home/ada"),
            Path::new("/srv/Cellar/koi"),
        );
        assert!(
            own.unwrap()
                .unit()
                .contains("ExecStart=\"/srv/Cellar/koi\" _daemon\n")
        );
    }

    #[tokio::test]
    async fn the_config_switch_keeps_koi_from_installing_or_removing_the_service() {
        let fake = Fake::default();
        let off = converge(&fake, &service(&[]), 1, true, false, &Daemon::Absent).await;
        assert_eq!(off.state, State::Off);
        assert_eq!(*fake.unit.borrow(), None);
        assert!(off.text().contains("off in config.toml"), "{}", off.text());
        fake.running.set(true);
        *fake.unit.borrow_mut() = Some(String::new());
        converge(&fake, &service(&[]), 0, true, false, &Daemon::Absent).await;
        assert!(fake.running.get());
    }

    #[tokio::test]
    async fn without_a_service_manager_the_link_says_so_and_what_to_do() {
        let fake = Fake {
            unavailable: true,
            ..Fake::default()
        };
        let report = converge(&fake, &service(&[]), 1, false, false, &Daemon::Absent).await;
        assert_eq!(report.state, State::Unavailable);
        assert_eq!(report.problem.as_deref(), Some("no user bus"));
        let text = report.text();
        assert!(
            text.starts_with("Koidrive can't sync in the background: no user bus.\n"),
            "{text}"
        );
        assert!(text.contains("koi service start"), "{text}");
        assert_eq!(
            converge(&fake, &service(&[]), 0, false, false, &Daemon::Absent)
                .await
                .text(),
            ""
        );
    }

    #[tokio::test]
    async fn lingering_is_turned_on_or_the_command_to_turn_it_on_is_named() {
        let allowed = Fake::default();
        allowed.lingers.set(Some(false));
        let report = converge(&allowed, &service(&[]), 1, false, false, &Daemon::Absent).await;
        assert_eq!(report.lingers, Some(true));
        let text = report.text();
        assert!(text.contains("keeps running after you log out"), "{text}");
        let refused = Fake {
            linger_refused: true,
            ..Fake::default()
        };
        refused.lingers.set(Some(false));
        let report = converge(&refused, &service(&[]), 1, false, false, &Daemon::Absent).await;
        assert_eq!(report.lingers, Some(false));
        assert!(
            report.text().contains("sudo loginctl enable-linger"),
            "{}",
            report.text()
        );
        assert!(report.status_line().contains("sudo loginctl enable-linger"));
    }

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn the_newest_binary_runs_the_service_by_semver_precedence() {
        let order = [
            "0.1.0-alpha.2",
            "0.1.0-alpha.10",
            "0.1.0-rc.1",
            "0.1.0",
            "0.1.1",
        ]
        .map(version);
        for (older, newer) in order.iter().zip(&order[1..]) {
            let keeps = |installed: &Version, caller: &Version| {
                target(Some(installed), caller, &Daemon::Absent).keep
            };
            assert!(keeps(newer, older), "{newer} over {older}");
            assert!(!keeps(older, newer), "{newer} over {older}");
            assert!(
                keeps(newer, newer),
                "the same version keeps the service as it is"
            );
        }
        let caller = version("0.1.0-alpha.2");
        assert!(
            target(
                Some(&version("0.1.0-alpha.2+build.7")),
                &caller,
                &Daemon::Absent
            )
            .keep
        );
        assert!(!target(None, &caller, &Daemon::Absent).keep);
    }

    #[test]
    fn a_running_daemon_older_than_the_binary_the_service_runs_or_without_a_version_restarts() {
        let restarts = |installed: Option<&str>, caller: &str, daemon: Daemon| {
            target(installed.map(version).as_ref(), &version(caller), &daemon).restart
        };
        let running = |text: &str| Daemon::Running(Some(version(text)));
        assert!(restarts(None, "0.1.0", running("0.1.0-rc.1")));
        assert!(!restarts(None, "0.1.0", running("0.1.0")));
        assert!(!restarts(None, "0.1.0", running("0.1.1")));
        assert!(restarts(None, "0.1.0", Daemon::Running(None)));
        assert!(!restarts(None, "0.1.0", Daemon::Absent));
        assert!(restarts(Some("0.1.1"), "0.1.0", running("0.1.0")));
        assert!(!restarts(Some("0.1.1"), "0.1.0", running("0.1.1")));
        assert!(!restarts(
            Some("0.1.0-alpha.2"),
            "0.1.0-alpha.10",
            running("0.1.0-alpha.10")
        ));
        assert!(restarts(
            Some("0.1.0-alpha.2"),
            "0.1.0-alpha.10",
            running("0.1.0-alpha.2")
        ));
    }

    #[test]
    fn the_binary_a_unit_or_a_launch_agent_runs_reads_back_as_written() {
        let home = "/home/ada";
        for binary in [
            "/home/ada/.local/bin/koi",
            "/opt/my $bin/100% \"a\\b\"/<k&i>",
        ] {
            let service =
                Service::new(&paths(home, &[]), Path::new(home), Path::new(binary)).unwrap();
            assert_eq!(unit_binary(&service.unit()).as_deref(), Some(binary));
            assert_eq!(plist_binary(&service.plist()).as_deref(), Some(binary));
        }
        assert_eq!(unit_binary("[Service]\n"), None);
    }

    fn koi_printing(dir: &Path, name: &str, printed: &str, status: u8) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(
            &path,
            format!("#!/bin/sh\nprintf '{printed}'\nexit {status}\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn running_on(fake: &Fake, binary: &Path) {
        let home = "/home/ada";
        let installed = Service::new(&paths(home, &[]), Path::new(home), binary).unwrap();
        *fake.unit.borrow_mut() = Some(installed.unit());
        fake.running.set(true);
    }

    fn runs(fake: &Fake) -> String {
        unit_binary(fake.unit.borrow().as_deref().unwrap()).unwrap()
    }

    #[test]
    fn mounted_and_translocated_binaries_are_volatile_but_installed_and_development_paths_are_not()
    {
        for binary in [
            "/Volumes/Koidrive/Koidrive.app/Contents/Helpers/koi",
            "/var/folders/ab/cd/T/AppTranslocation/123/d/Koidrive.app/Contents/Helpers/koi",
            "/private/var/folders/ab/cd/T/AppTranslocation/123/d/Koidrive.app/Contents/Helpers/koi",
        ] {
            assert!(volatile_binary(Path::new(binary)), "{binary}");
        }
        for binary in [
            "/Applications/Koidrive.app/Contents/Helpers/koi",
            "/Users/ada/Applications/Koidrive.app/Contents/Helpers/koi",
            "/Users/ada/koidrive/target/debug/koi",
            "/Users/ada/Volumes/Koidrive.app/Contents/Helpers/koi",
            "/Volumes-backup/Koidrive.app/Contents/Helpers/koi",
            "/Applications/AppTranslocation.app/Contents/Helpers/koi",
        ] {
            assert!(!volatile_binary(Path::new(binary)), "{binary}");
        }

        let dir = tempfile::tempdir().unwrap();
        let translocated = dir.path().join("AppTranslocation/koi");
        fs::create_dir_all(translocated.parent().unwrap()).unwrap();
        fs::write(&translocated, "").unwrap();
        let alias = dir.path().join("koi");
        std::os::unix::fs::symlink(&translocated, &alias).unwrap();
        assert!(volatile_binary(&alias));
    }

    #[tokio::test]
    async fn a_volatile_helper_cannot_install_a_service_or_replace_one_with_another_volatile_path()
    {
        let caller = Service {
            binary: "/Volumes/Koidrive/Koidrive.app/Contents/Helpers/koi".into(),
            ..service(&[])
        };
        let fake = Fake::default();
        let report = converge(&fake, &caller, 1, false, true, &Daemon::Absent).await;
        assert_eq!(report.state, State::NotInstalled);
        assert_eq!(report.problem.as_deref(), Some(MOVE_TO_APPLICATIONS));
        assert!(report.text().contains("Move Koidrive to Applications"));
        assert!(fake.unit.borrow().is_none());

        running_on(
            &fake,
            Path::new("/Volumes/Older/Koidrive.app/Contents/Helpers/koi"),
        );
        let before = fake.unit.borrow().clone();
        let report = converge(&fake, &caller, 1, false, true, &Daemon::Absent).await;
        assert_eq!(report.problem.as_deref(), Some(MOVE_TO_APPLICATIONS));
        assert_eq!(*fake.unit.borrow(), before);
        assert_eq!(fake.restarts.get(), 0);
    }

    #[tokio::test]
    async fn a_volatile_helper_keeps_an_older_stable_service_and_can_start_it_without_repointing_it()
     {
        let dir = tempfile::tempdir().unwrap();
        let stable = koi_printing(dir.path(), "koi", "koi 0.0.1\n", 0);
        let fake = Fake::default();
        running_on(&fake, &stable);
        let before = fake.unit.borrow().clone();
        for binary in [
            "/Volumes/Koidrive/Koidrive.app/Contents/Helpers/koi",
            "/private/var/folders/ab/cd/T/AppTranslocation/123/d/Koidrive.app/Contents/Helpers/koi",
        ] {
            let caller = Service {
                binary: binary.into(),
                ..service(&[])
            };
            let daemon = Daemon::Running(Some(version("0.0.1")));
            for _ in 0..2 {
                let report = converge(&fake, &caller, 1, false, false, &daemon).await;
                assert_eq!(report.state, State::Running);
                assert!(report.problem.is_none());
                assert_eq!(*fake.unit.borrow(), before);
            }
            assert_eq!(fake.restarts.get(), 0);

            fake.running.set(false);
            fake.switched_off.set(true);
            let left = converge(&fake, &caller, 1, false, false, &Daemon::Absent).await;
            assert_eq!(left.state, State::SwitchedOff);
            let started = converge(&fake, &caller, 1, false, true, &Daemon::Absent).await;
            assert_eq!((started.state, started.started), (State::Running, true));
            assert!(started.problem.is_none());
            assert_eq!(*fake.unit.borrow(), before);
        }
    }

    #[tokio::test]
    async fn a_stable_helper_replaces_a_volatile_service_even_when_it_reports_a_newer_version() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("AppTranslocation")).unwrap();
        let volatile = koi_printing(dir.path(), "AppTranslocation/koi", "koi 99.0.0\n", 0);
        let fake = Fake::default();
        running_on(&fake, &volatile);
        let caller = service(&[]);
        assert_eq!(newer_installed(&fake, &caller), None);
        let report = converge(&fake, &caller, 1, false, false, &Daemon::Absent).await;
        assert_eq!(report.state, State::Running);
        assert!(report.problem.is_none());
        assert_eq!(runs(&fake), caller.binary);
        assert_eq!(fake.restarts.get(), 1);
    }

    #[tokio::test]
    async fn a_service_on_a_newer_koi_keeps_it_and_one_on_an_older_or_broken_koi_moves_to_this_one()
    {
        let dir = tempfile::tempdir().unwrap();
        let caller = service(&[]);
        let this = this_version();
        let newer = koi_printing(dir.path(), "newer", "koi 99.0.0\n", 0);
        let same = koi_printing(dir.path(), "same", &format!("koi {this}\n"), 0);
        let fake = Fake::default();
        for (kept, running) in [(&newer, version("99.0.0")), (&same, this)] {
            running_on(&fake, kept);
            let report = converge(
                &fake,
                &caller,
                1,
                false,
                false,
                &Daemon::Running(Some(running)),
            )
            .await;
            assert_eq!(report.state, State::Running);
            assert_eq!(runs(&fake), kept.to_str().unwrap());
        }
        assert_eq!(fake.restarts.get(), 0);

        let not_executable = dir.path().join("not-executable");
        fs::write(&not_executable, "#!/bin/sh\necho 'koi 99.0.0'\n").unwrap();
        for moved in [
            koi_printing(dir.path(), "older", "koi 0.0.0-0\n", 0),
            koi_printing(dir.path(), "failing", "koi 99.0.0\n", 1),
            koi_printing(dir.path(), "other", "something 99.0.0\n", 0),
            koi_printing(dir.path(), "unparsable", "koi 99\n", 0),
            not_executable,
            dir.path().join("missing"),
        ] {
            running_on(&fake, &moved);
            let restarts = fake.restarts.get();
            converge(&fake, &caller, 1, false, false, &Daemon::Absent).await;
            assert_eq!(
                runs(&fake),
                "/home/ada/.local/bin/koi",
                "{}",
                moved.display()
            );
            assert_eq!(fake.restarts.get(), restarts + 1, "{}", moved.display());
        }
    }

    #[tokio::test]
    async fn a_service_whose_daemon_is_older_than_its_binary_restarts_once_and_a_stopped_one_only_starts()
     {
        let fake = Fake::default();
        let caller = service(&[]);
        let this = this_version();
        converge(&fake, &caller, 1, false, false, &Daemon::Absent).await;
        let older = Daemon::Running(Some(version("0.0.0-0")));
        converge(&fake, &caller, 1, false, false, &older).await;
        assert_eq!(fake.restarts.get(), 1);
        converge(&fake, &caller, 1, false, false, &Daemon::Running(None)).await;
        assert_eq!(fake.restarts.get(), 2);
        converge(
            &fake,
            &caller,
            1,
            false,
            false,
            &Daemon::Running(Some(this)),
        )
        .await;
        assert_eq!(fake.restarts.get(), 2);
        fake.running.set(false);
        let started = converge(&fake, &caller, 1, false, false, &older).await;
        assert_eq!((started.state, started.started), (State::Running, true));
        assert_eq!(fake.restarts.get(), 2);
    }

    #[tokio::test]
    async fn a_daemon_too_old_to_report_a_version_restarts_once_and_then_reports_the_binarys() {
        let dir = tempfile::tempdir().unwrap();
        let this = this_version();
        let same = koi_printing(dir.path(), "same", &format!("koi {this}\n"), 0);
        let fake = Fake::default();
        running_on(&fake, &same);
        converge(
            &fake,
            &service(&[]),
            1,
            false,
            false,
            &Daemon::Running(None),
        )
        .await;
        for _ in 0..3 {
            let restarted = Daemon::Running(Some(this.clone()));
            converge(&fake, &service(&[]), 1, false, false, &restarted).await;
        }
        assert_eq!(runs(&fake), same.to_str().unwrap());
        assert_eq!(fake.restarts.get(), 1);
    }

    #[tokio::test]
    async fn resume_without_a_daemon_starts_the_service_implicitly() {
        let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
        assert_eq!(
            ipc::run(&paths, Request::Pause).await.unwrap(),
            Response::Paused
        );
        assert_eq!(crate::resume(&paths).await.unwrap().text, "Resumed sync.\n");
        assert!(!paths.paused().exists());
        assert!(!paths.socket().exists());
        let fake = Fake::default();
        let caller = service(&[]);
        let mut daemon = None;
        let start_daemon = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !fake.running.get() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("implicit reconciliation starts the service");
            let driver = koidrive_driver::Driver::open(paths.clone()).unwrap();
            driver.start();
            daemon = Some(tokio::spawn(ipc::serve(
                koidrive_ipc::bind(&paths.socket()).unwrap(),
                driver,
            )));
        };
        let (report, ()) = tokio::join!(
            reconcile_with(&fake, Ok(caller), &paths, false),
            start_daemon
        );
        assert_eq!(
            (report.state, report.started, report.problem),
            (State::Running, true, None)
        );
        let daemon = daemon.unwrap();
        daemon.abort();
        let _ = daemon.await;
    }

    #[tokio::test]
    async fn resume_without_a_daemon_preserves_disabled_login_items() {
        let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
        assert_eq!(
            ipc::run(&paths, Request::Pause).await.unwrap(),
            Response::Paused
        );
        crate::resume(&paths).await.unwrap();
        let fake = Fake::default();
        let caller = service(&[]);
        *fake.unit.borrow_mut() = Some(caller.unit());
        fake.switched_off.set(true);

        let report = reconcile_with(&fake, Ok(caller), &paths, false).await;

        assert_eq!(
            (report.state, report.started, report.problem),
            (State::SwitchedOff, false, None)
        );
        assert!(!fake.running.get());
        assert!(fake.switched_off.get());
        assert!(!paths.paused().exists());
        assert!(!paths.socket().exists());
    }

    #[tokio::test]
    async fn reconciling_leaves_an_agent_switched_off_in_login_items_off_and_no_reconcile_resumes_a_pause()
     {
        let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
        let paused = ipc::run(&paths, Request::Pause).await.unwrap();
        assert_eq!(paused, Response::Paused);
        let fake = Fake::default();
        let caller = service(&[]);
        *fake.unit.borrow_mut() = Some(caller.unit());
        fake.switched_off.set(true);
        let report = reconcile_with(&fake, Ok(caller.clone()), &paths, false).await;
        assert_eq!(report.state, State::SwitchedOff);
        assert!(!fake.running.get());

        let driver = koidrive_driver::Driver::open(paths.clone()).unwrap();
        driver.start();
        let daemon = tokio::spawn(ipc::serve(
            koidrive_ipc::bind(&paths.socket()).unwrap(),
            driver.clone(),
        ));
        let report = reconcile_with(&fake, Ok(caller), &paths, true).await;
        assert_eq!((report.state, report.problem), (State::Running, None));
        let Response::Status { status } = ipc::run(&paths, Request::Status).await.unwrap() else {
            panic!("the daemon sent no status");
        };
        assert!(status.running && status.paused, "{status:?}");
        assert!(paths.paused().exists());
        daemon.abort();
    }

    fn unreadable_stores(paths: &Paths) {
        let store = paths.store(crate::ipc::tests::SPACE).unwrap();
        for suffix in ["-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", store.display()));
        }
        fs::write(&store, b"a store only a newer koi reads").unwrap();
    }

    #[tokio::test]
    async fn an_older_koi_hands_the_service_to_the_newer_installed_one_without_opening_the_profile()
    {
        let (dir, paths, _root) = crate::ipc::tests::linked_profile();
        unreadable_stores(&paths);
        let ran = dir.path().join("ran");
        let newer = dir.path().join("newer-koi");
        fs::write(
            &newer,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && echo 'koi 99.0.0' && exit 0\necho \"$@\" > '{}'\necho '{{\"data\":{{\"service\":{{\"manager\":\"launchd\",\"name\":\"dev.koidrive.koi\",\"state\":\"running\"}}}}}}'\n",
                ran.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&newer, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let fake = Fake::default();
        running_on(&fake, &newer);
        fake.running.set(false);
        for (asked, args) in [
            (false, "_reconcile --no-app --json"),
            (true, "service start --json"),
        ] {
            let report = reconcile_with(&fake, Ok(service(&[])), &paths, asked).await;
            assert_eq!(
                (report.state, report.name.as_str(), report.problem),
                (State::Running, "dev.koidrive.koi", None)
            );
            assert_eq!(fs::read_to_string(&ran).unwrap(), format!("{args}\n"));
            assert!(!fake.running.get());
            assert_eq!(runs(&fake), newer.to_str().unwrap());
        }

        let reporting = |name: &str, envelope: &str, status: u8| {
            let path = dir.path().join(name);
            fs::write(
                &path,
                format!("#!/bin/sh\n[ \"$1\" = --version ] && echo 'koi 99.0.0' && exit 0\necho '{envelope}'\nexit {status}\n"),
            )
            .unwrap();
            fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            path
        };
        let unknown = r#"{"data":{"service":{"manager":"launchd","name":"dev.koidrive.koi","state":"degraded"}},"error":null}"#;
        let degraded = reporting("degraded-koi", unknown, 0);
        running_on(&fake, &degraded);
        let report = reconcile_with(&fake, Ok(service(&[])), &paths, true).await;
        assert_eq!(
            (report.state, report.problem.as_deref()),
            (State::Running, None)
        );
        assert_eq!(report.name, fake.name(&service(&[])));
        assert!(!report.failed());
        let refused =
            r#"{"data":{"service":{"state":"degraded"}},"error":{"message":"it is degraded"}}"#;
        running_on(&fake, &reporting("refusing-koi", refused, 1));
        let report = reconcile_with(&fake, Ok(service(&[])), &paths, true).await;
        let problem = report.problem.unwrap();
        assert!(
            problem.contains("refusing-koi service start failed: it is degraded"),
            "{problem}"
        );

        let silent = koi_printing(dir.path(), "silent-koi", "koi 99.0.0\n", 0);
        running_on(&fake, &silent);
        let report = reconcile_with(&fake, Ok(service(&[])), &paths, true).await;
        assert_eq!(report.state, State::Running);
        assert!(!report.text().contains("Log in"), "{}", report.text());
        let problem = report.problem.unwrap();
        assert!(
            problem.contains("silent-koi service start reported no service"),
            "{problem}"
        );
    }

    #[tokio::test]
    async fn with_a_daemon_running_reconciling_reads_the_status_from_its_socket_and_opens_no_store()
    {
        let (dir, paths, root) = crate::ipc::tests::linked_profile();
        unreadable_stores(&paths);
        let folder: koidrive_driver::FolderStatus = serde_json::from_value(serde_json::json!({
            "space_id": crate::ipc::tests::SPACE,
            "space_name": "Projects",
            "path": root,
            "state": "up_to_date",
            "history_id": null,
            "last_seq": null,
            "head_seq": null,
            "remote_nodes": 0,
            "local_entries": null,
            "questions": 0,
            "problem": null,
        }))
        .unwrap();
        let status = koidrive_driver::Status {
            running: true,
            version: Some(this_version().to_string()),
            folders: vec![folder],
            ..koidrive_driver::Status::default()
        };
        let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
        let daemon = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                koidrive_ipc::read_line(reader).await.unwrap();
                let response = Response::Status {
                    status: status.clone(),
                };
                let _ = koidrive_ipc::write_line(&mut writer, &response).await;
            }
        });
        let this = this_version();
        let same = koi_printing(dir.path(), "same-koi", &format!("koi {this}\n"), 0);
        let fake = Fake::default();
        running_on(&fake, &same);
        let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
        assert_eq!((report.state, report.problem), (State::Running, None));
        assert_eq!(runs(&fake), same.to_str().unwrap());
        assert_eq!(fake.restarts.get(), 0);
        daemon.abort();
    }

    #[tokio::test]
    async fn the_service_lock_is_held_while_a_koi_picks_and_starts_the_binary() {
        let (dir, paths, root) = crate::ipc::tests::linked_profile();
        let folder: koidrive_driver::FolderStatus = serde_json::from_value(serde_json::json!({
            "space_id": crate::ipc::tests::SPACE,
            "space_name": "Projects",
            "path": root,
            "state": "up_to_date",
            "history_id": null,
            "last_seq": null,
            "head_seq": null,
            "remote_nodes": 0,
            "local_entries": null,
            "questions": 0,
            "problem": null,
        }))
        .unwrap();
        let status = koidrive_driver::Status {
            running: true,
            version: Some(this_version().to_string()),
            folders: vec![folder],
            ..koidrive_driver::Status::default()
        };
        let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
        let daemon = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                koidrive_ipc::read_line(reader).await.unwrap();
                let response = Response::Status {
                    status: status.clone(),
                };
                let _ = koidrive_ipc::write_line(&mut writer, &response).await;
            }
        });
        let older = koi_printing(dir.path(), "older-koi", "koi 0.0.0-0\n", 0);
        let fake = Fake {
            service_lock: Some(paths.service_lock()),
            ..Fake::default()
        };
        running_on(&fake, &older);
        let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
        assert_eq!((report.state, report.problem), (State::Running, None));
        assert_eq!(fake.locked_at_start.get(), Some(true));
        let file = fs::File::open(paths.service_lock()).unwrap();
        assert!(
            rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
        );
        daemon.abort();
    }

    #[tokio::test]
    async fn a_service_lock_another_koi_holds_past_the_deadline_is_a_problem() {
        let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
        let held = (fs::File::options().create(true).truncate(false).write(true))
            .open(paths.service_lock())
            .unwrap();
        let held = Flock::take(held).unwrap().unwrap();
        let began = Instant::now();
        let problem = lock_service(&paths, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(began.elapsed() >= Duration::from_secs(1));
        assert!(problem.contains("another koi has held"), "{problem}");
        drop(held);
        assert!(
            lock_service(&paths, Duration::from_secs(1))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waiting_for_the_service_lock_lets_the_other_tasks_of_the_command_run() {
        let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
        let held = (fs::File::options().create(true).truncate(false).write(true))
            .open(paths.service_lock())
            .unwrap();
        let _held = Flock::take(held).unwrap().unwrap();
        let ticks = Cell::new(0);
        let ticking = async {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                ticks.set(ticks.get() + 1);
            }
        };
        let waited = tokio::select! {
            waited = lock_service(&paths, Duration::from_secs(1)) => waited,
            () = ticking => unreachable!(),
        };
        assert!(waited.is_err());
        assert!(
            ticks.get() >= 5,
            "{} ticks while the lock was held",
            ticks.get()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waiting_for_a_delegated_koi_lets_the_interrupt_branch_run() {
        let dir = tempfile::tempdir().unwrap();
        let stalled = dir.path().join("stalled-koi");
        fs::write(&stalled, "#!/bin/sh\nexec sleep 30\n").unwrap();
        fs::set_permissions(
            &stalled,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let fake = Fake::default();
        let service = service(&[]);
        tokio::select! {
            biased;
            _ = delegated(&fake, &service, &stalled, false, Duration::from_millis(200)) => {
                panic!("the delegated child wait blocked the interrupt branch");
            }
            () = tokio::task::yield_now() => {}
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_delegation_kills_and_reaps_the_child_in_the_callers_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let stalled = dir.path().join("stalled-koi");
        fs::write(
            &stalled,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(
            &stalled,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let fake = Fake::default();
        let service = service(&[]);
        let mut delegated = Box::pin(delegated(&fake, &service, &stalled, false, DELEGATED));
        let pid = tokio::select! {
            biased;
            report = &mut delegated => panic!("delegation finished before cancellation: {report:?}"),
            pid = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(pid) = fs::read_to_string(&pid_file)
                        && let Ok(pid) = pid.trim().parse::<i32>()
                    {
                        break rustix::process::Pid::from_raw(pid).unwrap();
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }) => pid.expect("the delegated child started"),
        };
        let group = rustix::process::getpgid(Some(pid));
        drop(delegated);
        tokio::time::timeout(Duration::from_secs(5), async {
            while rustix::process::test_kill_process(pid) != Err(rustix::io::Errno::SRCH) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancellation killed and reaped the delegated child");
        assert_eq!(group, Ok(rustix::process::getpgrp()));
        assert_eq!(
            rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG).unwrap_err(),
            rustix::io::Errno::CHILD
        );
    }

    #[tokio::test]
    async fn a_delegated_koi_that_closes_stdout_still_has_to_exit_before_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let stalled = dir.path().join("stalled-koi");
        fs::write(
            &stalled,
            format!(
                "#!/bin/sh\nexec 1>&-\necho $$ > '{}'\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(
            &stalled,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let report = delegated(
            &Fake::default(),
            &service(&[]),
            &stalled,
            false,
            Duration::from_secs(1),
        )
        .await;
        let pid = rustix::process::Pid::from_raw(
            fs::read_to_string(pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            rustix::process::test_kill_process(pid),
            Err(rustix::io::Errno::SRCH)
        );
        assert!(
            report
                .problem
                .unwrap()
                .contains("did not finish within 1 seconds")
        );
    }

    #[tokio::test]
    async fn a_delegated_koi_that_exits_cannot_leave_its_stdout_wait_unbounded() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let holder_file = dir.path().join("holder");
        let stalled = dir.path().join("stalled-koi");
        fs::write(
            &stalled,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nsleep 30 &\necho $! > '{}'\n",
                pid_file.display(),
                holder_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(
            &stalled,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let report = tokio::time::timeout(
            Duration::from_secs(5),
            delegated(
                &Fake::default(),
                &service(&[]),
                &stalled,
                false,
                Duration::from_secs(1),
            ),
        )
        .await;
        let holder = rustix::process::Pid::from_raw(
            fs::read_to_string(holder_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        )
        .unwrap();
        let _ = rustix::process::kill_process(holder, rustix::process::Signal::KILL);
        let pid = rustix::process::Pid::from_raw(
            fs::read_to_string(pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG).unwrap_err(),
            rustix::io::Errno::CHILD
        );
        assert!(
            report
                .unwrap()
                .problem
                .unwrap()
                .contains("did not finish within 1 seconds")
        );
    }

    #[tokio::test]
    async fn a_delegated_report_keeps_the_folders_the_newer_koi_counted_and_this_profiles_log() {
        let dir = tempfile::tempdir().unwrap();
        let newer = dir.path().join("newer-koi");
        fs::write(
            &newer,
            r#"#!/bin/sh
echo '{"data":{"service":{"manager":"systemd","name":"koidrive.service","state":"stopped","folders":1}}}'
"#,
        )
        .unwrap();
        fs::set_permissions(&newer, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let fake = Fake::default();
        let service = service(&[]);
        let report = delegated(&fake, &service, &newer, false, PROBE).await;
        let text = report.text();
        assert!(text.contains("is installed but not running"), "{text}");
        assert!(
            text.contains(&service.log(ServiceManager::Systemd)),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_newer_koi_that_does_not_finish_is_killed_and_reported_after_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("pid");
        let stalled = dir.path().join("stalled-koi");
        fs::write(
            &stalled,
            format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n", pid.display()),
        )
        .unwrap();
        fs::set_permissions(
            &stalled,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let began = Instant::now();
        let report = delegated(
            &Fake::default(),
            &service(&[]),
            &stalled,
            true,
            Duration::from_secs(1),
        )
        .await;
        assert!(began.elapsed() < Duration::from_secs(3));
        let problem = report.problem.unwrap();
        assert!(
            problem.contains("stalled-koi service start did not finish within 1 seconds"),
            "{problem}"
        );
        let pid = fs::read_to_string(&pid).unwrap();
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!alive.success(), "the child was killed and reaped");
    }

    #[tokio::test]
    async fn a_daemon_that_takes_the_status_request_and_never_answers_is_a_problem_after_the_probe()
    {
        let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
        let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
        let daemon = tokio::spawn(async move {
            let _held = listener.accept().await.unwrap();
            std::future::pending::<()>().await
        });
        let fake = Fake::default();
        let began = Instant::now();
        let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
        assert!(began.elapsed() < PROBE + Duration::from_secs(2));
        assert_eq!(
            report.problem.as_deref(),
            Some("the daemon did not answer within 5 seconds")
        );
        assert_ne!(report.state, State::Unavailable);
        assert!(!report.text().contains("Log in"), "{}", report.text());
        daemon.abort();
    }

    #[tokio::test]
    async fn a_daemon_that_starts_and_then_never_replies_is_a_problem_after_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let _held = koidrive_driver::Driver::open(paths.clone()).unwrap();
        let socket = paths.socket();
        let daemon = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let listener = koidrive_ipc::bind(&socket).unwrap();
            let _accepted = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let started = Instant::now();
        let result = daemon_status(&paths).await;
        daemon.abort();
        assert!(started.elapsed() < Duration::from_secs(1) + PROBE + Duration::from_secs(2));
        assert_eq!(
            result.unwrap_err(),
            "the daemon did not answer within 5 seconds"
        );
    }

    #[test]
    fn launchd_reports_a_restarting_service_as_starting_and_a_crashing_one_as_stopped() {
        let job = |state: &str, exit: &str| {
            format!(
                "gui/501/dev.koidrive.koi = {{\n\tstate = {state}\n\truns = 2\n\t{exit}\n\tendpoints = {{\n\t\tstate = active\n\t}}\n}}\n"
            )
        };
        assert_eq!(
            launchd_state(&job("running", "last exit code = 1")),
            State::Running
        );
        assert_eq!(
            launchd_state(&job("SIGTERMed", "last exit code = (never exited)")),
            State::Starting
        );
        assert_eq!(
            launchd_state(&job("spawn scheduled", "last exit code = 0")),
            State::Starting
        );
        assert_eq!(
            launchd_state(&job(
                "spawn scheduled",
                "last terminating signal = Terminated: 15"
            )),
            State::Starting
        );
        assert_eq!(
            launchd_state(&job("spawn scheduled", "last exit code = 1")),
            State::Stopped
        );
        assert_eq!(launchd_state(&job("not running", "")), State::Stopped);
        assert_eq!(
            launchd_exit(&job("running", "last exit code = 78")).as_deref(),
            Some("exit code 78")
        );
        assert!(plan(1, State::Starting, false) == Action::Leave);
    }

    #[tokio::test]
    async fn a_daemon_that_keeps_exiting_is_a_stopped_service_that_names_its_exit_and_log() {
        let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
        let fake = Fake {
            crashed: Some("exit code 1"),
            ..Fake::default()
        };
        let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
        assert_eq!(report.state, State::Stopped);
        let problem = report.problem.clone().unwrap();
        assert!(
            problem.starts_with("its daemon keeps exiting (exit code 1); its log: "),
            "{problem}"
        );
        assert!(report.failed());
        assert!(!report.retryable());
    }

    #[test]
    fn only_a_running_service_whose_daemon_has_a_problem_is_worth_retrying() {
        let fake = Fake::default();
        let problem = |state| Report {
            problem: Some("the daemon did not answer within 5 seconds".into()),
            ..Report::new(&fake, String::new(), state)
        };
        assert!(problem(State::Running).retryable());
        assert!(!problem(State::Stopped).retryable());
        assert!(!problem(State::Unavailable).retryable());
        assert!(!Report::new(&fake, String::new(), State::Running).retryable());
    }

    #[tokio::test]
    async fn a_daemon_still_starting_after_a_restart_is_waited_for_and_not_a_problem() {
        let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
        let driver = koidrive_driver::Driver::open(paths.clone()).unwrap();
        let socket = paths.socket();
        let daemon = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            ipc::serve(koidrive_ipc::bind(&socket).unwrap(), driver).await;
        });
        let fake = Fake::default();
        let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
        assert_eq!(
            (report.state, report.problem.as_deref()),
            (State::Running, None)
        );
        assert!(!report.failed());
        daemon.abort();
    }

    #[test]
    fn a_koi_that_leaves_a_child_holding_its_output_reads_as_unversioned_within_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let koi = dir.path().join("lingering-koi");
        fs::write(&koi, "#!/bin/sh\necho 'koi 99.0.0'\nsleep 20 &\n").unwrap();
        fs::set_permissions(&koi, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let began = Instant::now();
        assert_eq!(version_of(&koi), None);
        assert!(began.elapsed() < PROBE + Duration::from_secs(2));
    }

    #[test]
    fn a_folder_macos_keeps_from_the_daemon_names_the_settings_and_the_restart() {
        let text = blocked_text(&[
            Blocked {
                folder: "/Users/ada/Documents/Work".into(),
                waiting: true,
            },
            Blocked {
                folder: "/Users/ada/Desktop/Notes".into(),
                waiting: false,
            },
        ]);
        assert!(
            text.contains(
                "koi is still waiting to read /Users/ada/Documents/Work. If macOS asks whether koi may access it, click Allow"
            ),
            "{text}"
        );
        assert!(
            text.contains("Privacy & Security > Files & Folders"),
            "{text}"
        );
        assert!(
            text.contains("koi folder restart /Users/ada/Desktop/Notes"),
            "{text}"
        );
    }

    #[test]
    fn the_restart_command_for_a_folder_with_a_space_pastes_as_one_argument() {
        let text = blocked_text(&[Blocked {
            folder: "/Users/ada/Desktop/Work Notes".into(),
            waiting: false,
        }]);
        assert!(
            text.contains("Then run: koi folder restart '/Users/ada/Desktop/Work Notes'\n"),
            "{text}"
        );
    }
}
