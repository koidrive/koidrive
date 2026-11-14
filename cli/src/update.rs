// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, SystemTime};

use koidrive_driver::Paths;
use koidrive_driver::update::{self, Release};
use koidrive_ipc::Code;
use semver::Version;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use url::Url;

use crate::error::CliError;
use crate::report::{Done, UpdateNoticeData, UpdateResult, UpdatedData, Via};
use crate::service;

const INSTALLER: Duration = Duration::from_secs(5 * 60);
const RECONCILE: Duration = Duration::from_secs(7 * 60);
const SCAN: Duration = Duration::from_secs(60 * 60);
const INSTALL_SCRIPT: &str = "https://koidrive.com/install.sh";
const INSTALL_SCRIPT_VARIABLE: &str = "KOIDRIVE_INSTALL_SCRIPT_URL";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installs {
    homebrew: Vec<PathBuf>,
    script: Option<PathBuf>,
    apps: Vec<PathBuf>,
}

impl Installs {
    pub fn of_this_machine() -> Installs {
        let absolute = |name| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let prefix = absolute("HOMEBREW_PREFIX");
        let home = absolute("HOME");
        let installs = Installs::under(prefix.as_deref(), home.as_deref());
        #[cfg(feature = "test-install-inventory")]
        let installs = {
            let mut installs = installs;
            if let Some(root) = absolute("KOIDRIVE_TEST_INSTALL_INVENTORY") {
                installs.homebrew = prefix
                    .into_iter()
                    .map(|prefix| prefix.join("opt/koidrive/bin/koi"))
                    .collect();
                installs.apps = vec![root.join("Applications/Koidrive.app/Contents/Helpers/koi")];
            }
            installs
        };
        installs
    }

    fn under(homebrew_prefix: Option<&Path>, home: Option<&Path>) -> Installs {
        let mut homebrew: Vec<PathBuf> = Vec::new();
        let prefixes = [
            homebrew_prefix,
            Some(Path::new("/opt/homebrew")),
            Some(Path::new("/usr/local")),
            Some(Path::new("/home/linuxbrew/.linuxbrew")),
        ];
        for prefix in prefixes.into_iter().flatten() {
            let koi = prefix.join("opt/koidrive/bin/koi");
            if !homebrew.contains(&koi) {
                homebrew.push(koi);
            }
        }
        Installs {
            homebrew,
            script: home.map(|home| home.join(".local/bin/koi")),
            apps: std::iter::once(PathBuf::from(
                "/Applications/Koidrive.app/Contents/Helpers/koi",
            ))
            .chain(home.map(|home| home.join("Applications/Koidrive.app/Contents/Helpers/koi")))
            .collect(),
        }
    }

    fn all(&self) -> Vec<PathBuf> {
        let mut all = self.homebrew.clone();
        all.extend(self.script.clone());
        all.extend(self.apps.iter().cloned());
        all
    }
}

pub fn homebrew_link(binary: &Path) -> Option<PathBuf> {
    if binary.ends_with("opt/koidrive/bin/koi") {
        return Some(binary.to_path_buf());
    }
    if !binary.ends_with("bin/koi") {
        return None;
    }
    let version = binary.parent()?.parent()?;
    let formula = version.parent()?;
    let cellar = formula.parent()?;
    let prefix = cellar.parent()?;
    let named = |path: &Path, name: &str| path.file_name().is_some_and(|file| file == name);
    (named(formula, "koidrive") && named(cellar, "Cellar") && version.file_name().is_some())
        .then(|| prefix.join("opt/koidrive/bin/koi"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Method {
    Cask { brew: PathBuf },
    Formula { brew: PathBuf },
    Installer,
    App,
    AppDownload,
    Development,
}

impl Method {
    fn via(&self) -> Option<Via> {
        match self {
            Method::Cask { .. } | Method::Formula { .. } => Some(Via::Homebrew),
            Method::Installer => Some(Via::Installer),
            Method::App | Method::AppDownload => Some(Via::App),
            Method::Development => None,
        }
    }
}

pub(crate) struct Install {
    binary: PathBuf,
    method: Method,
}

impl Install {
    pub(crate) fn current() -> Result<Self, CliError> {
        let installs = Installs::of_this_machine();
        let mut prefixes: Vec<PathBuf> = installs
            .homebrew
            .iter()
            .filter_map(|koi| koi.ancestors().nth(4).map(Path::to_path_buf))
            .collect();
        #[cfg(feature = "test-install-inventory")]
        let isolated = std::env::var_os("KOIDRIVE_TEST_INSTALL_INVENTORY").is_some();
        #[cfg(not(feature = "test-install-inventory"))]
        let isolated = false;
        if !isolated && let Some(path) = std::env::var_os("PATH") {
            prefixes.extend(
                std::env::split_paths(&path)
                    .filter(|bin| {
                        bin.is_absolute() && bin.file_name().is_some_and(|name| name == "bin")
                    })
                    .filter_map(|bin| bin.parent().map(Path::to_path_buf)),
            );
        }
        let binary = std::env::current_exe()
            .map_err(|error| failed(format!("Cannot locate this koi: {error}")))?;
        let invoked = std::env::args_os()
            .next()
            .and_then(|arg| Self::invoked(&binary, &arg, std::env::var_os("PATH").as_deref()));
        let install = Self::resolve(
            invoked.as_deref().unwrap_or(&binary),
            env!("CARGO_PKG_VERSION"),
            &prefixes,
            &installs.apps,
        )?;
        if invoked.is_none()
            && matches!(
                install.method,
                Method::Installer | Method::App | Method::AppDownload
            )
        {
            return Err(failed(
                "Cannot identify the invoked koi; run its full path to update".to_owned(),
            ));
        }
        Ok(install)
    }

    fn invoked(binary: &Path, arg: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
        if arg.is_empty() {
            return None;
        }
        let invoked = Path::new(arg);
        let invoked = if invoked.is_absolute() || invoked.components().count() > 1 {
            invoked.to_path_buf()
        } else {
            on_path(arg, path)?
        };
        (fs::canonicalize(&invoked).ok()? == fs::canonicalize(binary).ok()?).then_some(invoked)
    }

    fn resolve(
        binary: &Path,
        version: &str,
        prefixes: &[PathBuf],
        apps: &[PathBuf],
    ) -> Result<Self, CliError> {
        if update::is_development_version(version) {
            return Ok(Install {
                binary: binary.to_path_buf(),
                method: Method::Development,
            });
        }
        let resolved = fs::canonicalize(binary)
            .map_err(|error| failed(format!("Cannot resolve {}: {error}", binary.display())))?;
        let mut candidates = Vec::new();
        let mut link = binary.to_path_buf();
        for _ in 0..40 {
            if let Some(prefix) =
                homebrew_link(&link).and_then(|opt| opt.ancestors().nth(4).map(Path::to_path_buf))
            {
                candidates.push(prefix);
            }
            if let Some(prefix) = link
                .parent()
                .filter(|bin| bin.ends_with("bin"))
                .and_then(Path::parent)
            {
                candidates.push(prefix.to_path_buf());
            }
            let Ok(target) = fs::read_link(&link) else {
                break;
            };
            link = match target.is_absolute() {
                true => target,
                false => link.parent().unwrap_or(Path::new("/")).join(target),
            };
        }
        candidates.extend_from_slice(prefixes);
        let formula = candidates.iter().find(|prefix| {
            fs::canonicalize(prefix.join("opt/koidrive/bin/koi"))
                .is_ok_and(|helper| helper == resolved)
        });
        let method = if let Some(prefix) = formula {
            Method::Formula {
                brew: fs::canonicalize(prefix)
                    .map_err(|error| {
                        failed(format!(
                            "Cannot resolve Homebrew at {}: {error}",
                            prefix.display()
                        ))
                    })?
                    .join("bin/brew"),
            }
        } else if let Some(opt) = homebrew_link(&resolved) {
            Method::Formula {
                brew: opt
                    .ancestors()
                    .nth(4)
                    .expect("a formula path has a prefix")
                    .join("bin/brew"),
            }
        } else if resolved.ends_with("Koidrive.app/Contents/Helpers/koi") {
            let owned = candidates.iter().find(|prefix| {
                fs::read_dir(prefix.join("Caskroom/koidrive")).is_ok_and(|entries| {
                    entries.flatten().any(|entry| {
                        fs::canonicalize(entry.path().join("Koidrive.app/Contents/Helpers/koi"))
                            .is_ok_and(|helper| helper == resolved)
                    })
                })
            });
            match owned {
                Some(prefix) => Method::Cask {
                    brew: fs::canonicalize(prefix)
                        .map_err(|error| {
                            failed(format!(
                                "Cannot resolve Homebrew at {}: {error}",
                                prefix.display()
                            ))
                        })?
                        .join("bin/brew"),
                },
                None if apps.iter().any(|helper| {
                    fs::canonicalize(helper).is_ok_and(|helper| helper == resolved)
                }) =>
                {
                    Method::App
                }
                None => Method::AppDownload,
            }
        } else if resolved.ends_with("bin/koi")
            && resolved
                .ancestors()
                .nth(2)
                .is_some_and(|keg| keg.join("INSTALL_RECEIPT.json").is_file())
        {
            return Err(failed("Cannot locate the Homebrew that owns this koi; set its HOMEBREW_PREFIX or put its bin directory on PATH".to_owned()));
        } else if resolved.file_name().is_some_and(|name| name == "koi")
            && !resolved
                .components()
                .any(|part| part.as_os_str().to_string_lossy().ends_with(".app"))
        {
            Method::Installer
        } else {
            return Err(failed(format!(
                "{} is not a recognized Koidrive installation",
                resolved.display()
            )));
        };
        Ok(Install {
            binary: resolved,
            method,
        })
    }

    fn command(&self) -> Option<Vec<&str>> {
        match &self.method {
            Method::Cask { brew } | Method::Formula { brew } => {
                let path = std::env::var_os("PATH");
                let selected = on_path(OsStr::new("brew"), path.as_deref()).and_then(|selected| {
                    selected.parent().and_then(|bin| fs::canonicalize(bin).ok())
                });
                let owned = brew.parent().and_then(|bin| fs::canonicalize(bin).ok());
                let executable = if owned.is_some() && selected == owned {
                    "brew"
                } else {
                    brew.to_str()?
                };
                let mut command = vec![executable, "upgrade"];
                if matches!(self.method, Method::Cask { .. }) {
                    command.push("--cask");
                }
                command.push("koidrive");
                Some(command)
            }
            Method::Installer | Method::App => Some(vec![self.binary.to_str()?, "update"]),
            Method::AppDownload | Method::Development => None,
        }
    }

    pub(crate) fn notice(&self, update: &update::Update, version: &str) -> UpdateNoticeData {
        let command = self.command().map(|command| command_text(&command));
        let update = if self.method == Method::Development {
            &update::Update::Off
        } else {
            update
        };
        UpdateNoticeData::of(update, version, command.as_deref())
    }
}

fn command_text(command: &[&str]) -> String {
    command
        .iter()
        .map(|word| crate::pairing::shell_word(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn on_path(name: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|bin| bin.join(Path::new(name)))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

fn refuse_root(root: bool) -> Result<(), CliError> {
    match root {
        true => Err(CliError::usage(
            "koi update upgrades Koidrive for one user; run it as your own user, not as root",
        )),
        false => Ok(()),
    }
}

struct Echo {
    json: bool,
    log: Option<File>,
}

#[derive(Clone, Copy)]
enum Child {
    Progress,
    Reconcile,
}

impl Echo {
    fn open(path: &Path, json: bool) -> Echo {
        let log = (path.parent().map(fs::create_dir_all).transpose().ok())
            .and_then(|_| File::create(path).ok());
        Echo { json, log }
    }

    fn line(&mut self, line: &str, stderr: bool) {
        match self.json || stderr {
            true => eprintln!("{line}"),
            false => println!("{line}"),
        }
        if let Some(log) = &mut self.log {
            let _ = writeln!(log, "{line}");
        }
    }

    async fn run(
        &mut self,
        mut command: Command,
        within: Duration,
        kind: Child,
    ) -> io::Result<Option<(ExitStatus, Vec<u8>, String)>> {
        let shown = format!("{:?}", command.as_std());
        if let Some(log) = &mut self.log {
            let _ = writeln!(log, "$ {shown}");
        }
        let kill_on_drop = match kind {
            Child::Progress => true,
            Child::Reconcile => {
                command.process_group(0);
                false
            }
        };
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(kill_on_drop)
            .spawn()?;
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();
        let mut stderr = BufReader::new(child.stderr.take().expect("stderr is piped")).lines();
        let finished = tokio::time::timeout(within, async {
            let (mut printed, mut errors) = (Vec::new(), String::new());
            let (mut out_open, mut err_open) = (true, true);
            while out_open || err_open {
                tokio::select! {
                    line = stdout.next_line(), if out_open => match line? {
                        Some(line) => {
                            match kind {
                                Child::Progress => self.line(&line, false),
                                Child::Reconcile => {
                                    if let Some(log) = &mut self.log {
                                        let _ = writeln!(log, "{line}");
                                    }
                                }
                            }
                            printed.extend_from_slice(line.as_bytes());
                            printed.push(b'\n');
                        }
                        None => out_open = false,
                    },
                    line = stderr.next_line(), if err_open => match line? {
                        Some(line) => {
                            self.line(&line, true);
                            errors.push_str(&line);
                            errors.push('\n');
                        }
                        None => err_open = false,
                    },
                }
            }
            let status = child.wait().await?;
            io::Result::Ok((status, printed, errors))
        })
        .await;
        match finished {
            Ok(finished) => finished.map(Some),
            Err(_) => {
                if let Some(log) = &mut self.log {
                    let _ = writeln!(log, "did not finish within {} seconds", within.as_secs());
                }
                Ok(None)
            }
        }
    }
}

fn failed(why: String) -> CliError {
    CliError::new(Code::Failed, why)
}

fn minutes(within: Duration) -> u64 {
    within.as_secs().div_ceil(60)
}

fn exit(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit status {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        _ => status.to_string(),
    }
}

fn script_url() -> Result<(Url, bool), CliError> {
    let text = std::env::var(INSTALL_SCRIPT_VARIABLE).unwrap_or_else(|_| INSTALL_SCRIPT.to_owned());
    let url = Url::parse(&text).map_err(|_| failed(format!("{text} is not a URL")))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    match url.scheme() {
        "https" => Ok((url, true)),
        "http" if loopback => Ok((url, false)),
        _ => Err(failed(format!("{text} is not an https URL"))),
    }
}

async fn installer(
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

fn log_path(paths: &Paths) -> PathBuf {
    let home = (std::env::var_os("HOME").map(PathBuf::from)).filter(|home| home.is_absolute());
    match (cfg!(target_os = "macos"), home) {
        (true, Some(home)) => home.join("Library/Logs/Koidrive/update.log"),
        _ => paths.state_dir.join("update.log"),
    }
}

enum UpdateBinary {
    Other(PathBuf),
    #[cfg(target_os = "macos")]
    App(Box<crate::app_install::VerifiedHelper>),
}

#[derive(Debug)]
enum ReconcileError {
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
    fn path(&self) -> &Path {
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
async fn discover_app_version(
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
async fn select_app_helper(
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

pub async fn run(paths: &Paths, json: bool) -> Result<Done, CliError> {
    refuse_root(rustix::process::geteuid().is_root())?;
    let install = Install::current()?;
    if install.method == Method::Development {
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
    if matches!(install.method, Method::Cask { .. } | Method::Formula { .. }) {
        let command = install.command().ok_or_else(|| {
            failed("Homebrew's path cannot be represented as a shell command".to_owned())
        })?;
        return Ok(Done::ok(
            UpdatedData {
                from: env!("CARGO_PKG_VERSION").to_owned(),
                to: env!("CARGO_PKG_VERSION").to_owned(),
                via: install.method.via(),
                app: None,
                result: UpdateResult::Unchanged,
                service: None,
            },
            format!(
                "Koidrive is managed by Homebrew. To update, run: {}\n",
                command_text(&command)
            ),
        )
        .unchanged()
        .then(&command));
    }
    let release = update::check_now(paths)
        .await
        .map_err(|error| CliError::new(Code::Unavailable, error.to_string()))?
        .ok_or_else(|| failed("GitHub lists no Koidrive release for this system".to_owned()))?;
    let Release { version, url } = release;
    let to = Version::parse(&version).map_err(|_| {
        failed(format!(
            "GitHub names the release {version}, which is not a version"
        ))
    })?;
    if install.method == Method::AppDownload {
        let from = service::this_version();
        if to.cmp_precedence(&from).is_gt() {
            return Err(failed(format!("Download Koidrive {to} from {url}")));
        }
        return Ok(Done::ok(
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
        .unchanged());
    }
    let manager = service::system();
    let service = service::this_service(paths);
    let app_deadline = tokio::time::Instant::now() + RECONCILE;
    #[cfg(target_os = "macos")]
    let existing_app = if install.method == Method::App {
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
            error
                .message
                .push_str(&format!("\nDownload Koidrive {to} from {url}"));
            error
        })?)
    } else {
        None
    };
    let from = match install.method {
        #[cfg(target_os = "macos")]
        Method::App => existing_app.as_ref().unwrap().1.clone(),
        #[cfg(not(target_os = "macos"))]
        Method::App => service::this_version(),
        _ => service::version_of(&install.binary).unwrap_or_else(service::this_version),
    };
    let upgrading = to.cmp_precedence(&from).is_gt();
    let download = || failed(format!("Download Koidrive {to} from {url}"));
    let mut echo = Echo::open(&log_path(paths), json);
    let koi = match install.method {
        Method::App => None,
        _ => Some(UpdateBinary::Other(install.binary.clone())),
    };
    #[cfg(target_os = "macos")]
    let mut koi = koi;
    let prepared_app: Option<crate::app::Prepared> = None;
    #[cfg(target_os = "macos")]
    let mut prepared_app = prepared_app;
    match install.method {
        Method::App => {
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
                    Ok(Some(helper)) => koi = Some(UpdateBinary::App(Box::new(helper))),
                    Ok(None) => {
                        return Err(app_not_updated(
                            paths,
                            &from,
                            &to,
                            &url,
                            prepared,
                            app_deadline,
                        )
                        .await);
                    }
                    Err(error) => {
                        return Err(app_not_updated(
                            paths,
                            &from,
                            &to,
                            &url,
                            prepared.invalidate(error),
                            app_deadline,
                        )
                        .await);
                    }
                }
                prepared_app = Some(prepared);
            }
            #[cfg(not(target_os = "macos"))]
            return Err(app_not_updated(paths, &from, &to, &url, prepared, app_deadline).await);
        }
        Method::Installer if upgrading => {
            let dir = install.binary.parent().ok_or_else(download)?;
            installer(script_url()?, dir, &to, &mut echo).await?
        }
        Method::Installer => (),
        Method::Cask { .. }
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
    let reconciled = reconcile_updated(
        &koi,
        &to,
        manager.as_ref(),
        service,
        &mut echo,
        reconcile_deadline,
    )
    .await;
    let (actual, report, app) =
        finish_reconciliation(paths, prepared_app, &from, reconciled, app_deadline).await?;
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

fn updated(
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

fn update_problem(report: &service::Report, app: Option<&crate::app::Report>) -> Option<String> {
    if report.problem.is_some()
        || matches!(
            report.state,
            service::State::Stopped | service::State::Unavailable
        )
    {
        return Some(report.failure_message());
    }
    app.and_then(crate::app::Report::problem).map(str::to_owned)
}

async fn finish_reconciliation(
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

async fn app_not_updated(
    paths: &Paths,
    from: &Version,
    target: &Version,
    url: &str,
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
    let guidance = format!("Download Koidrive {target} from {url}");
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

async fn reconcile_updated(
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
        )).into());
    }
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

fn scan(
    seen: &mut HashMap<PathBuf, (SystemTime, Option<Version>)>,
    candidates: &[PathBuf],
    runs: &Version,
) -> Option<PathBuf> {
    if update::is_development_version(&runs.to_string()) {
        return None;
    }
    (candidates.iter())
        .filter_map(|koi| {
            let Ok(modified) = fs::metadata(koi).and_then(|metadata| metadata.modified()) else {
                seen.remove(koi);
                return None;
            };
            let cached = seen
                .entry(koi.clone())
                .or_insert_with(|| (modified, service::version_of(koi)));
            if cached.0 != modified {
                *cached = (modified, service::version_of(koi));
            }
            Some((cached.1.clone()?, koi))
        })
        .filter(|(version, _)| version.cmp_precedence(runs).is_gt())
        .max_by(|(a, _), (b, _)| a.cmp_precedence(b))
        .map(|(_, koi)| koi.clone())
}

fn take_service(koi: &Path) {
    let mut command = Command::new(koi);
    command
        .arg("_reconcile")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(koi = %koi.display(), %error, "a newer koi is installed, but it did not run");
            return;
        }
    };
    tracing::info!(koi = %koi.display(), "a newer koi is installed; it takes the background service");
    let koi = koi.to_path_buf();
    tokio::spawn(async move {
        match tokio::time::timeout(RECONCILE, child.wait()).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => {
                tracing::warn!(koi = %koi.display(), %status, "the newer koi did not take the background service")
            }
            Ok(Err(error)) => {
                tracing::warn!(koi = %koi.display(), %error, "the newer koi did not take the background service")
            }
            Err(_) => {
                tracing::warn!(koi = %koi.display(), "the newer koi did not take the background service within seven minutes")
            }
        }
    });
}

pub async fn follow_newest(paths: Paths) {
    if update::is_development_version(env!("CARGO_PKG_VERSION")) {
        return;
    }
    let mut seen = HashMap::new();
    let mut every = tokio::time::interval(SCAN);
    loop {
        every.tick().await;
        let paths = paths.clone();
        let looked = tokio::task::spawn_blocking(move || {
            let manager = service::system();
            let installed = (service::this_service(&paths).ok())
                .and_then(|service| manager.installed(&service))
                .map(PathBuf::from);
            let newer = installed.and_then(|installed| {
                let mut candidates = Installs::of_this_machine().all();
                let mut running = vec![installed];
                running.extend(std::env::current_exe().ok());
                candidates.extend(running.iter().filter_map(|koi| homebrew_link(koi)));
                candidates.extend(running);
                scan(&mut seen, &candidates, &service::this_version())
            });
            (seen, newer)
        })
        .await;
        let Ok((kept, newer)) = looked else { return };
        seen = kept;
        if let Some(koi) = newer {
            take_service(&koi);
        }
    }
}

#[cfg(test)]
#[path = "../tests/support/executable.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn executable(path: &Path, script: &str) {
        super::fixture::executable(path, script.as_bytes());
    }

    #[test]
    fn the_running_binary_determines_its_installation_method() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("standalone/koi");
        let app = dir
            .path()
            .join("Applications/Koidrive.app/Contents/Helpers/koi");
        let custom = dir
            .path()
            .join("Custom Apps/Koidrive.app/Contents/Helpers/koi");
        let formula = dir.path().join("brew/Cellar/koidrive/0.1.0/bin/koi");
        executable(&script, "#!/bin/sh\nexit 0\n");
        executable(&app, "#!/bin/sh\nexit 0\n");
        let script_command = format!("{} update", fs::canonicalize(&script).unwrap().display());
        let app_command = format!("{} update", fs::canonicalize(&app).unwrap().display());
        let formula_brew = fs::canonicalize(dir.path()).unwrap().join("brew/bin/brew");
        let formula_command = format!("{} upgrade koidrive", formula_brew.display());
        for (binary, method, command) in [
            (&script, Method::Installer, Some(script_command.as_str())),
            (&app, Method::App, Some(app_command.as_str())),
            (&custom, Method::AppDownload, None),
            (
                &formula,
                Method::Formula { brew: formula_brew },
                Some(formula_command.as_str()),
            ),
        ] {
            executable(binary, "#!/bin/sh\nexit 0\n");
            let install =
                Install::resolve(binary, "0.1.0", &[], std::slice::from_ref(&app)).unwrap();
            assert_eq!(install.method, method);
            assert_eq!(install.binary, fs::canonicalize(binary).unwrap());
            assert_eq!(
                install.command().map(|args| args.join(" ")).as_deref(),
                command
            );
            let available = update::Update::Available {
                latest: "0.2.0".into(),
                url: "https://github.com/koidrive/koidrive/releases/tag/v0.2.0".into(),
                checked_at_ms: 1,
            };
            assert_eq!(
                install.notice(&available, "0.1.0").command.as_deref(),
                command
            );
            let dev =
                Install::resolve(binary, "0.0.0-dev", &[], std::slice::from_ref(&app)).unwrap();
            assert_eq!(dev.method, Method::Development);
            assert_eq!(dev.command(), None);
            assert_eq!(dev.method.via(), None);
            assert_eq!(
                dev.notice(&available, "0.0.0-dev").state,
                crate::report::UpdateState::Off
            );
        }
        assert_eq!(
            Installs::under(Some(Path::new("/opt/homebrew")), None)
                .homebrew
                .len(),
            3
        );
    }

    #[test]
    fn an_app_update_notice_names_the_resolved_helper() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("My Apps/Koidrive.app/Contents/Helpers/koi");
        executable(&binary, "#!/bin/sh\nexit 0\n");
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&binary, &alias).unwrap();
        let install =
            Install::resolve(&alias, "0.1.0", &[], std::slice::from_ref(&binary)).unwrap();
        let available = update::Update::Available {
            latest: "0.2.0".into(),
            url: "https://github.com/koidrive/koidrive/releases/tag/v0.2.0".into(),
            checked_at_ms: 1,
        };
        let resolved = fs::canonicalize(binary).unwrap();
        assert_eq!(
            install.notice(&available, "0.1.0").command,
            Some(format!(
                "{} update",
                crate::pairing::shell_word(resolved.to_str().unwrap())
            ))
        );
    }

    #[test]
    fn a_homebrew_keg_maps_to_its_opt_link_under_every_prefix() {
        for prefix in [
            "/opt/homebrew",
            "/usr/local",
            "/home/linuxbrew/.linuxbrew",
            "/srv/brew",
        ] {
            let opt = Path::new(prefix).join("opt/koidrive/bin/koi");
            assert_eq!(homebrew_link(&opt), Some(opt.clone()));
            for version in ["0.1.0", "0.1.0-alpha.3", "0.1.2_1"] {
                let keg = format!("{prefix}/Cellar/koidrive/{version}/bin/koi");
                assert_eq!(
                    homebrew_link(Path::new(&keg)),
                    Some(Path::new(prefix).join("opt/koidrive/bin/koi")),
                    "{keg}"
                );
            }
        }
        for other in [
            "/opt/homebrew/bin/koi",
            "/opt/homebrew/Cellar/other/0.1.0/bin/koi",
            "/opt/homebrew/Cellar/koidrive/0.1.0/libexec/koi",
            "/opt/homebrew/Cellar/koidrive/0.1.0/bin/koi2",
            "/home/ada/.local/bin/koi",
        ] {
            assert_eq!(homebrew_link(Path::new(other)), None, "{other}");
        }
    }

    #[test]
    fn koi_update_refuses_to_run_as_root() {
        let refused = refuse_root(true).unwrap_err();
        assert_eq!(refused.code, Code::Usage);
        assert!(
            refused.message.contains("run it as your own user"),
            "{}",
            refused.message
        );
        assert!(refuse_root(false).is_ok());
    }

    fn echo(dir: &Path) -> Echo {
        Echo::open(&dir.join("logs/update.log"), true)
    }

    #[test]
    fn user_applications_are_discovered_without_treating_their_helper_as_install_sh() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let user = home.join("Applications/Koidrive.app/Contents/Helpers/koi");
        executable(&user, "#!/bin/sh\nexit 0\n");
        let installs = Installs::under(None, Some(home));
        assert!(installs.all().contains(&user));
        let alias = home.join("koi");
        std::os::unix::fs::symlink(&user, &alias).unwrap();
        for binary in [&user, &alias] {
            let install = Install::resolve(binary, "0.1.0", &[], &installs.apps).unwrap();
            assert_eq!(install.method, Method::App);
            assert_eq!(install.binary, fs::canonicalize(&user).unwrap());
        }
        assert_eq!(
            installs.all().iter().filter(|path| **path == user).count(),
            1
        );
        let no_home = Installs::under(None, None);
        assert_eq!(no_home.apps.len(), 1);
        assert!(no_home.apps[0].is_absolute());
    }

    #[test]
    fn update_errors_name_the_failed_report_rather_than_success_or_retry_text() {
        for (state, service_problem, app_problem) in [
            ("stopped", None, None),
            ("unavailable", None, None),
            ("running", Some("Service reconciliation failed"), None),
            ("running", None, Some("App signature is invalid")),
            (
                "running",
                Some("Service reconciliation failed"),
                Some("App signature is invalid"),
            ),
            ("running", None, None),
            ("switched_off", None, None),
        ] {
            let report: service::Report = serde_json::from_value(serde_json::json!({
                "manager": "launchd", "name": "fixture", "state": state,
                "problem": service_problem
            }))
            .unwrap();
            let expected =
                if state == "stopped" || state == "unavailable" || service_problem.is_some() {
                    Some(report.failure_message())
                } else {
                    app_problem.map(str::to_owned)
                };
            for present in [true, false] {
                let app: crate::app::Report = if present {
                    serde_json::from_value(serde_json::json!({
                        "state": "present", "path": "/fixture/Koidrive.app", "version": "99.0.0",
                        "change": "installed", "launch": "requested", "problem": app_problem
                    }))
                    .unwrap()
                } else if let Some(problem) = app_problem {
                    crate::app::Report::Failed {
                        committed: Vec::new(),
                        retired: Vec::new(),
                        preserved: Vec::new(),
                        problem: problem.into(),
                        residue: Vec::new(),
                    }
                } else {
                    crate::app::Report::skipped("ci")
                };
                assert_eq!(
                    update_problem(&report, Some(&app)),
                    expected,
                    "{state}, {app:?}"
                );
            }
        }
    }

    fn app_fixture(
        dir: &Path,
        actual: &str,
        change: crate::app::Change,
        problem: Option<String>,
    ) -> crate::app::Prepared {
        let path = dir.join("user Applications/Koidrive.app");
        let binary = path.join("Contents/Helpers/koi");
        executable(
            &binary,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi {actual}' ;;\n_reconcile) echo '{{\"data\":{{\"service\":{{\"manager\":\"launchd\",\"name\":\"fixture\",\"state\":\"running\"}},\"app\":{{\"state\":\"skipped\",\"reason\":\"bundled_helper\"}}}}}}' ;;\nesac\n"
            ),
        );
        crate::app::Prepared::fixture(crate::app::Report::Present {
            committed: Vec::new(),
            path,
            version: "99.0.0".into(),
            change,
            launch: if problem.is_some() {
                crate::app::Launch::Failed
            } else {
                crate::app::Launch::NotRequested
            },
            problem,
            retired: Vec::new(),
            residue: Vec::new(),
            preserved: Vec::new(),
        })
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn selected_development_app_discovery_uses_its_helper_instead_of_the_invoking_app() {
        let (_system_fixture, system) =
            crate::app_install::verified_helper_fixture_with_metadata("98.2.0", None).await;
        let (_user_fixture, invoking) = crate::app_install::verified_helper_fixture("99.0.0").await;
        let installs = Installs {
            homebrew: Vec::new(),
            script: None,
            apps: vec![system.path().to_path_buf(), invoking.path().to_path_buf()],
        };
        assert!(installs.all().contains(&system.path().to_path_buf()));
        assert!(installs.all().contains(&invoking.path().to_path_buf()));
        let selected = Install::resolve(system.path(), "98.2.0", &[], &installs.apps).unwrap();
        assert_eq!(selected.method, Method::App);
        assert_eq!(selected.binary, fs::canonicalize(system.path()).unwrap());
        assert!(system.version().is_none());
        let deadline = tokio::time::Instant::now() + RECONCILE;
        system.check(deadline).await.unwrap();
        let actual = service::version_of(system.path()).unwrap();
        system.check(deadline).await.unwrap();
        assert_eq!(actual.to_string(), "98.2.0");
        let discovered = discover_app_version(&system, deadline).await.unwrap();
        assert_eq!(
            discovered, actual,
            "The selected system app differs from the invoking user app"
        );
        assert!(!invoking.path().with_extension("calls").exists());
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn current_ci_skip_reconciles_the_verified_existing_helper() {
        for (actual_version, state, problem, fails) in [
            ("99.0.0", "running", None, false),
            ("100.0.0", "switched_off", None, false),
            ("99.0.0", "off", None, false),
            ("99.0.0", "stopped", None, true),
            ("99.0.0", "unavailable", None, true),
            ("99.0.0", "switched_off", Some("registration failed"), true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let version = Version::parse(actual_version).unwrap();
            let target = Version::parse("99.0.0").unwrap();
            let prepared = crate::app::Prepared::fixture(crate::app::Report::skipped("ci"));
            assert!(prepared.helper().is_none());
            let (_fixture, helper) =
                crate::app_install::verified_helper_fixture(actual_version).await;
            let original = fs::read_to_string(helper.path()).unwrap();
            let fields = format!(
                "\"state\":{},\"problem\":{}",
                serde_json::to_string(state).unwrap(),
                serde_json::to_string(&problem).unwrap()
            );
            executable(
                helper.path(),
                &original.replace("\"state\":\"running\"", &fields),
            );
            let deadline = tokio::time::Instant::now() + RECONCILE;
            let from = discover_app_version(&helper, deadline).await.unwrap();
            assert_eq!(from, version);
            let helper = select_app_helper(&prepared, helper, &from, &target, deadline)
                .await
                .unwrap()
                .unwrap();
            let binary = UpdateBinary::App(Box::new(helper));
            let paths = Paths::under(dir.path());
            let manager = service::system();
            let reconciled = reconcile_updated(
                &binary,
                &target,
                manager.as_ref(),
                service::Service::new(&paths, dir.path(), binary.path()),
                &mut echo(dir.path()),
                deadline,
            )
            .await;
            let (actual, service, app) =
                finish_reconciliation(&paths, Some(prepared), &version, reconciled, deadline)
                    .await
                    .unwrap();
            assert_eq!(actual, version);
            assert_eq!(update_problem(&service, app.as_ref()).is_some(), fails);
            assert_eq!(serde_json::to_value(&service).unwrap()["state"], state);
            assert!(matches!(app, Some(crate::app::Report::Skipped { reason }) if reason == "ci"));
            assert_eq!(
                fs::read_to_string(binary.path().with_extension("calls")).unwrap(),
                "--version\n--version\n_reconcile --json\n"
            );
        }
    }

    #[tokio::test]
    async fn committed_app_failures_are_updated_partials_even_at_the_same_version() {
        let dir = tempfile::tempdir().unwrap();
        for from in ["1.0.0", "99.0.0"] {
            let app: crate::app::Report = serde_json::from_value(serde_json::json!({
                "state":"failed", "problem":"source changed",
                "committed":[{"path":dir.path().join("Koidrive.app"), "version":"99.0.0", "change":"replaced"}],
                "retired":[dir.path().join("retired.app")], "residue":[dir.path().join("receipt.json")]
            })).unwrap();
            let error = app_not_updated(
                &Paths::under(dir.path()),
                &Version::parse(from).unwrap(),
                &Version::parse("99.0.0").unwrap(),
                "https://example.test/release",
                crate::app::Prepared::fixture(app),
                tokio::time::Instant::now(),
            )
            .await;
            assert_eq!(error.code, Code::Failed);
            assert!(
                error
                    .message
                    .starts_with("source changed\nDownload Koidrive")
            );
            let partial = error.partial.unwrap();
            assert_eq!(partial.data["from"], from);
            assert_eq!(partial.data["to"], "99.0.0");
            assert_eq!(partial.data["result"], "updated");
            assert_ne!(serde_json::to_value(partial.outcome).unwrap(), "unchanged");
            assert_eq!(partial.data["app"]["state"], "failed");
            assert!(partial.data["app"].get("version").is_none());
            assert!(!partial.text.contains("was not updated"));
            assert!(!partial.text.contains("is installed"));
            assert_eq!(partial.text.matches("source changed").count(), 1);
            assert_eq!(partial.text.matches("Download Koidrive").count(), 1);
        }
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn ci_skip_fallback_requires_a_current_verified_app_and_never_accepts_errors() {
        let target = Version::parse("99.0.0").unwrap();
        for (reason, version, selected) in [
            ("ci", "98.0.0", false),
            ("ci", "100.0.0", true),
            ("platform", "99.0.0", false),
            ("architecture", "99.0.0", false),
        ] {
            let (_fixture, helper) = crate::app_install::verified_helper_fixture(version).await;
            let binary = helper.path().to_path_buf();
            let prepared = crate::app::Prepared::fixture(crate::app::Report::skipped(reason));
            let result = select_app_helper(
                &prepared,
                helper,
                &Version::parse(version).unwrap(),
                &target,
                tokio::time::Instant::now() + RECONCILE,
            )
            .await
            .unwrap();
            assert_eq!(result.is_some(), selected);
            assert!(!binary.with_extension("calls").exists());
        }
        let (_fixture, helper) = crate::app_install::verified_helper_fixture("99.0.0").await;
        let prepared = crate::app::Prepared::fixture(
            serde_json::from_value(serde_json::json!({"state":"failed", "problem":"offline"}))
                .unwrap(),
        );
        assert!(
            select_app_helper(
                &prepared,
                helper,
                &target,
                &target,
                tokio::time::Instant::now() + RECONCILE
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn initial_app_discovery_rejects_changes_during_the_version_probe() {
        for invalid in [
            "signer",
            "helper",
            "during_version",
            "during_helper",
            "invalid_version",
        ] {
            let (_fixture, helper) =
                crate::app_install::verified_helper_fixture_with_metadata("99.0.0", None).await;
            let binary = helper.path().to_path_buf();
            let bundle = binary.ancestors().nth(3).unwrap();
            match invalid {
                "signer" => fs::write(bundle.join("Signer"), "OTHERTEAM").unwrap(),
                "helper" => {
                    fs::rename(&binary, binary.with_extension("old")).unwrap();
                    executable(&binary, "#!/bin/sh\necho unsafe > \"$0.calls\"\n");
                }
                "during_version" => executable(
                    &binary,
                    &format!(
                        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\nprintf OTHERTEAM > '{}/Signer'\necho 'koi 98.0.0'\n",
                        bundle.display()
                    ),
                ),
                "during_helper" => executable(
                    &binary,
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\n/bin/mv \"$0\" \"$0.old\"\n/bin/cp \"$0.old\" \"$0\"\necho 'koi 98.0.0'\n",
                ),
                _ => executable(
                    &binary,
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\necho 'not-a-version'\n",
                ),
            }
            let error = discover_app_version(&helper, tokio::time::Instant::now() + RECONCILE)
                .await
                .unwrap_err();
            if invalid == "invalid_version" {
                assert!(
                    matches!(error, ReconcileError::Failed(error) if error.message.contains("during app discovery"))
                );
            } else {
                assert!(matches!(error, ReconcileError::Unverified(_)));
            }
            if matches!(
                invalid,
                "during_version" | "during_helper" | "invalid_version"
            ) {
                assert_eq!(
                    fs::read_to_string(binary.with_extension("calls")).unwrap(),
                    "--version\n"
                );
            } else {
                assert!(!binary.with_extension("calls").exists());
            }
        }
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn ci_skip_uses_the_discovered_version_instead_of_missing_or_different_metadata() {
        for (metadata, actual, selected) in [
            (None, "99.0.0", true),
            (None, "98.0.0", false),
            (Some("100.0.0"), "98.0.0", false),
        ] {
            let (_fixture, helper) =
                crate::app_install::verified_helper_fixture_with_metadata(actual, metadata).await;
            let binary = helper.path().to_path_buf();
            let deadline = tokio::time::Instant::now() + RECONCILE;
            let from = discover_app_version(&helper, deadline).await.unwrap();
            assert_eq!(from.to_string(), actual);
            let prepared = crate::app::Prepared::fixture(crate::app::Report::skipped("ci"));
            let helper = select_app_helper(
                &prepared,
                helper,
                &from,
                &Version::parse("99.0.0").unwrap(),
                deadline,
            )
            .await
            .unwrap();
            assert_eq!(helper.is_some(), selected);
            assert_eq!(
                fs::read_to_string(binary.with_extension("calls")).unwrap(),
                "--version\n"
            );
        }
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn partial_update_versions_require_commits_or_a_verified_target_helper() {
        for (actual, committed) in [
            ("invalid", false),
            ("98.0.0", false),
            ("99.0.0", false),
            ("100.0.0", false),
            ("invalid", true),
            ("98.0.0", true),
            ("99.0.0", true),
            ("100.0.0", true),
        ] {
            let (_fixture, helper) = crate::app_install::verified_helper_fixture("99.0.0").await;
            let binary = helper.path().to_path_buf();
            let bundle = binary.ancestors().nth(3).unwrap().to_path_buf();
            executable(&binary, &format!("#!/bin/sh\necho 'koi {actual}'\n"));
            let committed = if committed {
                serde_json::json!([{"path":bundle, "version":"99.0.0", "change":"replaced"}])
            } else {
                serde_json::json!([])
            };
            let prepared = crate::app::Prepared::fixture(serde_json::from_value(serde_json::json!({
                "state":"present", "path":bundle, "version":"99.0.0", "change":"unchanged", "launch":"not_requested", "committed":committed
            })).unwrap());
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let manager = service::system();
            let deadline = tokio::time::Instant::now() + RECONCILE;
            let reconciled = reconcile_updated(
                &UpdateBinary::App(Box::new(helper)),
                &Version::parse("99.0.0").unwrap(),
                manager.as_ref(),
                Err("The profile cannot construct its service".into()),
                &mut echo(dir.path()),
                deadline,
            )
            .await;
            let error = finish_reconciliation(
                &paths,
                Some(prepared),
                &Version::parse("1.0.0").unwrap(),
                reconciled,
                deadline,
            )
            .await
            .unwrap_err();
            let partial = error.partial.unwrap();
            assert_eq!(partial.data["to"], "99.0.0", "{actual}, {committed}");
            assert_eq!(partial.data["result"], "updated");
            assert_eq!(serde_json::to_value(partial.outcome).unwrap(), "ok");
            assert_eq!(
                partial.data["app"]["committed"]
                    .as_array()
                    .map(Vec::len)
                    .unwrap_or(0),
                committed.as_array().unwrap().len()
            );
            assert!(partial.data.get("service").is_none());
        }
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn failed_version_probes_cannot_preserve_a_substituted_app_as_present() {
        for actual in ["invalid", "98.0.0", "99.0.0"] {
            let (_fixture, helper) = crate::app_install::verified_helper_fixture("99.0.0").await;
            let binary = helper.path().to_path_buf();
            let bundle = binary.ancestors().nth(3).unwrap().to_path_buf();
            let prepared = crate::app::Prepared::fixture(serde_json::from_value(serde_json::json!({
                "state":"present", "path":bundle, "version":"99.0.0", "change":"unchanged", "launch":"not_requested"
            })).unwrap());
            executable(
                &binary,
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\nprintf OTHERTEAM > '{}/Signer'\necho 'koi {actual}'\n",
                    bundle.display()
                ),
            );
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let manager = service::system();
            let deadline = tokio::time::Instant::now() + RECONCILE;
            let reconciled = reconcile_updated(
                &UpdateBinary::App(Box::new(helper)),
                &Version::parse("99.0.0").unwrap(),
                manager.as_ref(),
                service::Service::new(&paths, dir.path(), &binary),
                &mut echo(dir.path()),
                deadline,
            )
            .await;
            assert!(
                matches!(&reconciled, Err(ReconcileError::Unverified(_))),
                "{actual}: {reconciled:?}"
            );
            let error = finish_reconciliation(
                &paths,
                Some(prepared),
                &Version::parse("1.0.0").unwrap(),
                reconciled,
                deadline,
            )
            .await
            .unwrap_err();
            let partial = error.partial.unwrap();
            assert_eq!(partial.data["app"]["state"], "failed");
            assert_eq!(partial.data["result"], "unchanged");
            assert_eq!(partial.data["to"], "1.0.0");
            assert!(!partial.text.contains("is installed"));
            assert_eq!(
                fs::read_to_string(binary.with_extension("calls")).unwrap(),
                "--version\n"
            );
        }
    }

    #[test]
    fn reused_apps_report_verified_version_advancement_without_a_namespace_commit() {
        for from in ["1.0.0", "99.0.0"] {
            for state in ["running", "switched_off", "stopped"] {
                let service: service::Report = serde_json::from_value(serde_json::json!({
                    "manager":"launchd", "name":"fixture", "state":state
                }))
                .unwrap();
                let app = serde_json::from_value(serde_json::json!({
                    "state":"present", "path":"/fixture/Koidrive.app", "version":"99.0.0", "change":"unchanged", "launch":"not_requested"
                })).unwrap();
                let result = updated(
                    &Version::parse(from).unwrap(),
                    Version::parse("99.0.0").unwrap(),
                    Via::App,
                    service,
                    Some(app),
                    false,
                );
                let done = if state == "stopped" {
                    result.unwrap_err().partial.map(|done| *done).unwrap()
                } else {
                    result.unwrap()
                };
                assert_eq!(done.data["from"], from);
                assert_eq!(done.data["to"], "99.0.0");
                assert_eq!(done.data["app"]["change"], "unchanged");
                assert_eq!(done.data["service"]["state"], state);
                if from == "1.0.0" {
                    assert_eq!(done.data["result"], "updated");
                    assert_ne!(serde_json::to_value(done.outcome).unwrap(), "unchanged");
                    assert!(
                        done.text
                            .starts_with("Updated Koidrive from 1.0.0 to 99.0.0.")
                    );
                } else {
                    assert_eq!(done.data["result"], "unchanged");
                    assert_eq!(serde_json::to_value(done.outcome).unwrap(), "unchanged");
                    assert!(
                        done.text
                            .starts_with("Koidrive 99.0.0 is the newest release.")
                    );
                }
            }
        }
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn substituted_app_helpers_are_rejected_at_each_execution_boundary() {
        for substitution in ["signer", "helper", "bundle", "during_version"] {
            let (_fixture, helper) = crate::app_install::verified_helper_fixture("99.0.0").await;
            let binary = helper.path().to_path_buf();
            let bundle = binary.ancestors().nth(3).unwrap().to_path_buf();
            let committed =
                serde_json::json!([{"path":bundle, "version":"99.0.0", "change":"replaced"}]);
            let prepared = crate::app::Prepared::fixture(serde_json::from_value(serde_json::json!({
                "state":"present", "path":bundle, "version":"99.0.0", "change":"replaced", "launch":"not_requested", "committed":committed,
            })).unwrap());
            match substitution {
                "signer" => fs::write(bundle.join("Signer"), "OTHERTEAM").unwrap(),
                "helper" => {
                    fs::rename(&binary, binary.with_extension("old")).unwrap();
                    executable(&binary, "#!/bin/sh\necho unsafe > \"$0.calls\"\n");
                }
                "during_version" => executable(
                    &binary,
                    &format!(
                        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) printf OTHERTEAM > '{}/Signer'; echo 'koi 99.0.0' ;;\n_reconcile) echo unsafe >> \"$0.calls\" ;;\nesac\n",
                        bundle.display()
                    ),
                ),
                _ => {
                    fs::rename(&bundle, bundle.with_extension("displaced")).unwrap();
                    fs::create_dir_all(binary.parent().unwrap()).unwrap();
                    executable(&binary, "#!/bin/sh\necho unsafe > \"$0.calls\"\n");
                }
            }
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let manager = service::system();
            let version = Version::parse("99.0.0").unwrap();
            let deadline = tokio::time::Instant::now() + RECONCILE;
            let reconciled = reconcile_updated(
                &UpdateBinary::App(Box::new(helper)),
                &version,
                manager.as_ref(),
                service::Service::new(&paths, dir.path(), &binary),
                &mut echo(dir.path()),
                deadline,
            )
            .await;
            assert!(matches!(&reconciled, Err(ReconcileError::Unverified(_))));
            let error = finish_reconciliation(
                &paths,
                Some(prepared),
                &Version::parse("1.0.0").unwrap(),
                reconciled,
                deadline,
            )
            .await
            .unwrap_err();
            let partial = error.partial.unwrap();
            assert_eq!(partial.data["result"], "updated");
            assert_eq!(partial.data["to"], "99.0.0");
            assert_eq!(partial.data["app"]["state"], "failed");
            assert_eq!(partial.data["app"]["committed"], committed);
            assert!(!partial.text.contains("is installed"));
            if substitution == "during_version" {
                assert_eq!(
                    fs::read_to_string(binary.with_extension("calls")).unwrap(),
                    "--version\n"
                );
            } else {
                assert!(!binary.with_extension("calls").exists());
            }
        }
    }

    #[tokio::test]
    async fn unsuccessful_app_preparation_preserves_its_report_in_an_unchanged_partial() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        for app in [
            crate::app::Report::Failed {
                committed: Vec::new(),
                retired: Vec::new(),
                preserved: Vec::new(),
                problem: "Checksum mismatch".into(),
                residue: vec![dir.path().join("retained-mount")],
            },
            crate::app::Report::skipped("ci"),
            crate::app::Report::skipped("platform"),
            crate::app::Report::skipped("architecture"),
        ] {
            let expected = serde_json::to_value(&app).unwrap();
            let prepared = crate::app::Prepared::fixture(app);
            assert_eq!(prepared.helper(), None);
            let error = app_not_updated(
                &paths,
                &Version::parse("1.0.0").unwrap(),
                &Version::parse("99.0.0").unwrap(),
                "https://example.test/releases/v99.0.0",
                prepared,
                tokio::time::Instant::now(),
            )
            .await;
            assert_eq!(error.code, Code::Failed);
            assert!(
                error.message.contains(
                    "Download Koidrive 99.0.0 from https://example.test/releases/v99.0.0"
                )
            );
            let partial = error.partial.unwrap();
            assert_eq!(serde_json::to_value(partial.outcome).unwrap(), "unchanged");
            assert_eq!(partial.data["from"], "1.0.0");
            assert_eq!(partial.data["to"], "1.0.0");
            assert_eq!(partial.data["result"], "unchanged");
            assert_eq!(partial.data["via"], "app");
            assert_eq!(partial.data["app"], expected);
            assert!(partial.data.get("service").is_none());
            assert!(!partial.text.contains("Updated Koidrive"));
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn an_app_reconciliation_error_retains_the_installed_app_in_its_partial_result() {
        for (actual, valid_service) in [("invalid", true), ("98.0.0", true), ("99.0.0", false)] {
            for result in [UpdateResult::Updated, UpdateResult::Unchanged] {
                let dir = tempfile::tempdir().unwrap();
                let paths = Paths::under(dir.path());
                let path = dir.path().join("Koidrive.app");
                let koi = path.join("Contents/Helpers/koi");
                let marker = dir.path().join("must-not-reconcile");
                executable(
                    &koi,
                    &format!(
                        "#!/bin/sh\ncase \"$1\" in\n--version) echo 'koi {actual}' ;;\n_reconcile) echo ran > '{}' ;;\nesac\n",
                        marker.display()
                    ),
                );
                let old = dir.path().join("retired-helper");
                let residue = dir.path().join("retained-residue");
                fs::write(&old, "old helper remains usable").unwrap();
                fs::write(&residue, "preserved residue").unwrap();
                let prepared = crate::app::Prepared::fixture(crate::app::Report::Present {
                    committed: Vec::new(),
                    path,
                    version: "99.0.0".into(),
                    change: crate::app::Change::Replaced,
                    launch: crate::app::Launch::NotRequested,
                    problem: None,
                    retired: vec![old.clone()],
                    residue: vec![residue.clone()],
                    preserved: Vec::new(),
                });
                let manager = service::system();
                let service = if valid_service {
                    service::Service::new(&paths, dir.path(), &koi)
                } else {
                    Err("The profile cannot construct its service".into())
                };
                let target = Version::parse("99.0.0").unwrap();
                let deadline = tokio::time::Instant::now() + RECONCILE;
                let reconciled = reconcile_updated(
                    &UpdateBinary::Other(koi.clone()),
                    &target,
                    manager.as_ref(),
                    service,
                    &mut echo(dir.path()),
                    deadline,
                )
                .await;
                assert!(reconciled.is_err());
                let from = Version::parse(if result == UpdateResult::Updated {
                    "1.0.0"
                } else {
                    "99.0.0"
                })
                .unwrap();
                let error =
                    finish_reconciliation(&paths, Some(prepared), &from, reconciled, deadline)
                        .await
                        .unwrap_err();
                let partial = error
                    .partial
                    .expect("the installed app must remain in the error result");
                assert_eq!(partial.data["app"]["state"], "present");
                assert_eq!(partial.data["app"]["version"], "99.0.0");
                assert_eq!(partial.data["app"]["problem"], error.message);
                assert_eq!(partial.data["app"]["retired"], serde_json::json!([old]));
                assert_eq!(partial.data["app"]["residue"], serde_json::json!([residue]));
                assert_eq!(partial.data["via"], "app");
                assert_eq!(partial.data["to"], "99.0.0");
                assert_eq!(
                    partial.data["result"],
                    serde_json::to_value(result).unwrap()
                );
                assert!(partial.data.get("service").is_none());
                if result == UpdateResult::Unchanged {
                    assert_eq!(serde_json::to_value(partial.outcome).unwrap(), "unchanged");
                }
                assert!(!marker.exists());
                assert_eq!(fs::read(old).unwrap(), b"old helper remains usable");
                assert_eq!(fs::read(residue).unwrap(), b"preserved residue");
            }
        }
    }

    #[tokio::test]
    async fn the_app_update_wait_uses_only_its_remaining_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let koi = dir.path().join("koi");
        let pid_file = dir.path().join("pid");
        executable(
            &koi,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n--version) echo 'koi 99.0.0' ;;\n_reconcile) echo $$ > '{}'; exec /bin/sleep 30 ;;\nesac\n",
                pid_file.display()
            ),
        );
        let paths = Paths::under(dir.path());
        let manager = service::system();
        let service = service::Service::new(&paths, dir.path(), &koi);
        let mut echo = echo(dir.path());
        let target = Version::parse("99.0.0").unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let binary = UpdateBinary::Other(koi.clone());
        let tail = reconcile_updated(
            &binary,
            &target,
            manager.as_ref(),
            service,
            &mut echo,
            deadline,
        );
        tokio::pin!(tail);
        let ready = async {
            loop {
                if let Some(pid) = fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|text| text.trim().parse::<i32>().ok())
                    .and_then(rustix::process::Pid::from_raw)
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let pid = tokio::select! {
            result = &mut tail => panic!("The reconciliation ended before the hanging helper started: {result:?}"),
            pid = ready => pid,
        };
        assert!(rustix::process::test_kill_process(pid).is_ok());
        let (_, report, app) = tokio::time::timeout(Duration::from_secs(5), tail)
            .await
            .unwrap()
            .unwrap();
        assert!(report.failed());
        assert!(app.is_none());
        assert!(tokio::time::Instant::now() >= deadline);
        assert!(rustix::process::test_kill_process(pid).is_ok());
        rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while rustix::process::test_kill_process(pid).is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let old = dir.path().join("retired-helper");
        fs::write(&old, "old helper remains usable").unwrap();
        let prepared = crate::app::Prepared::fixture(crate::app::Report::Present {
            committed: Vec::new(),
            path: dir.path().join("Koidrive.app"),
            version: "99.0.0".into(),
            change: crate::app::Change::Replaced,
            launch: crate::app::Launch::NotRequested,
            problem: None,
            retired: vec![old.clone()],
            residue: Vec::new(),
            preserved: Vec::new(),
        });
        let app = prepared.finish(&paths, Ok(&report), deadline).await;
        assert!(
            matches!(app, crate::app::Report::Present { problem: Some(_), retired, .. } if retired == vec![old.clone()])
        );
        assert_eq!(fs::read(old).unwrap(), b"old helper remains usable");
    }

    #[tokio::test]
    async fn app_updates_verify_the_prepared_helper_before_one_shared_reconcile() {
        for (change, actual) in [
            (crate::app::Change::Unchanged, "99.0.0"),
            (crate::app::Change::Replaced, "100.0.0"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let prepared = app_fixture(
                dir.path(),
                actual,
                change,
                Some("The menu bar app could not open".into()),
            );
            let koi = prepared.helper().unwrap();
            assert!(koi.starts_with(dir.path()));
            let paths = Paths::under(dir.path());
            let service = service::Service::new(&paths, dir.path(), &koi);
            let manager = service::system();
            let (version, report, forwarded) = reconcile_updated(
                &UpdateBinary::Other(koi.clone()),
                &Version::parse("99.0.0").unwrap(),
                manager.as_ref(),
                service,
                &mut echo(dir.path()),
                tokio::time::Instant::now() + RECONCILE,
            )
            .await
            .unwrap();
            assert_eq!(version.to_string(), actual);
            assert!(
                matches!(forwarded, Some(crate::app::Report::Skipped { reason }) if reason == "bundled_helper")
            );
            let before = serde_json::to_value(&report).unwrap();
            let app = prepared
                .finish(
                    &paths,
                    Ok(&report),
                    tokio::time::Instant::now() + Duration::from_secs(10),
                )
                .await;
            assert!(app.failed());
            assert_eq!(serde_json::to_value(&report).unwrap(), before);
            assert!(!report.failed());
            assert_eq!(
                fs::read_to_string(koi.with_extension("calls")).unwrap(),
                "--version\n_reconcile --json\n"
            );
        }
    }

    #[tokio::test]
    async fn a_prepared_app_does_not_bypass_actual_helper_version_verification() {
        for version in ["0.1.0", "not-semver"] {
            let dir = tempfile::tempdir().unwrap();
            let prepared = app_fixture(dir.path(), version, crate::app::Change::Replaced, None);
            let koi = prepared.helper().unwrap();
            let paths = Paths::under(dir.path());
            let service = service::Service::new(&paths, dir.path(), &koi);
            let manager = service::system();
            let error = reconcile_updated(
                &UpdateBinary::Other(koi.clone()),
                &Version::parse("99.0.0").unwrap(),
                manager.as_ref(),
                service,
                &mut echo(dir.path()),
                tokio::time::Instant::now() + RECONCILE,
            )
            .await
            .unwrap_err();
            assert!(matches!(error, ReconcileError::Failed(error) if error.code == Code::Failed));
            assert_eq!(
                fs::read_to_string(koi.with_extension("calls")).unwrap(),
                "--version\n"
            );
        }
    }

    #[tokio::test]
    async fn the_shared_update_tail_forwards_app_failure_without_changing_service() {
        for changed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let koi = dir.path().join("koi");
            let mut app = serde_json::json!({"state":"failed", "problem":"offline", "residue":["fixture-residue"]});
            if changed {
                app["committed"] = serde_json::json!([{"path":"/Applications/Koidrive.app", "version":"99.0.0", "change":"replaced"}]);
            }
            let report = serde_json::json!({"manager":"launchd", "name":"fixture", "state":"off"});
            let envelope = serde_json::json!({"data":{"service":report, "app":app}});
            executable(
                &koi,
                &format!(
                    "#!/bin/sh\ncase \"$1\" in\n--version) echo 'koi 99.0.0' ;;\n_reconcile) printf '%s\\n' '{envelope}' ;;\nesac\n"
                ),
            );
            let paths = Paths::under(dir.path());
            let manager = service::system();
            let (_, service, forwarded) = reconcile_updated(
                &UpdateBinary::Other(koi.clone()),
                &Version::parse("99.0.0").unwrap(),
                manager.as_ref(),
                service::Service::new(&paths, dir.path(), &koi),
                &mut echo(dir.path()),
                tokio::time::Instant::now() + RECONCILE,
            )
            .await
            .unwrap();
            assert_eq!(serde_json::to_value(service).unwrap(), report);
            let forwarded = forwarded.unwrap();
            assert_eq!(!forwarded.committed().is_empty(), changed);
            assert_eq!(serde_json::to_value(forwarded).unwrap(), app);
        }
    }

    #[test]
    fn reconcile_subprocess() {
        let Some(dir) = std::env::var_os("KOI_RECONCILE_TEST_DIR").map(PathBuf::from) else {
            return;
        };
        let mode = std::env::var("KOI_RECONCILE_TEST_MODE").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut command = Command::new(dir.join("koi"));
            command.arg(&dir);
            let mut echo = echo(&dir);
            if mode == "cancel" {
                assert!(
                    tokio::time::timeout(
                        Duration::from_millis(100),
                        echo.run(command, Duration::from_secs(10), Child::Reconcile)
                    )
                    .await
                    .is_err()
                );
            } else {
                let within = if mode == "timeout" {
                    Duration::from_millis(100)
                } else {
                    Duration::from_secs(10)
                };
                let result = echo.run(command, within, Child::Reconcile).await.unwrap();
                assert!(result.is_none());
            }
        });
    }

    fn surviving_reconcile(mode: &str) {
        let dir = tempfile::tempdir().unwrap();
        executable(
            &dir.path().join("koi"),
            "#!/bin/sh\ntouch \"$1/ready\"\ni=0\nwhile [ ! -f \"$1/continue\" ] && [ $i -lt 250 ]; do sleep 0.02; i=$((i+1)); done\n[ -f \"$1/continue\" ] && touch \"$1/completed\"\n",
        );
        let mut parent = std::process::Command::new(std::env::current_exe().unwrap());
        parent
            .args([
                "--exact",
                "update::tests::reconcile_subprocess",
                "--nocapture",
            ])
            .env("KOI_RECONCILE_TEST_DIR", dir.path())
            .env("KOI_RECONCILE_TEST_MODE", mode)
            .process_group(0);
        let mut parent = parent.spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !dir.path().join("ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(dir.path().join("ready").exists());
        if mode == "sigint" {
            assert!(
                std::process::Command::new("/bin/kill")
                    .args(["-INT", "--", &format!("-{}", parent.id())])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        loop {
            if let Some(status) = parent.try_wait().unwrap() {
                if mode == "sigint" {
                    assert!(!status.success());
                } else {
                    assert!(status.success());
                }
                break;
            }
            if std::time::Instant::now() >= deadline {
                parent.kill().unwrap();
                parent.wait().unwrap();
                panic!("foreground reconcile wait did not end");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        fs::write(dir.path().join("continue"), "").unwrap();
        while !dir.path().join("completed").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            dir.path().join("completed").exists(),
            "reconciliation must finish after {mode}"
        );
    }

    #[test]
    fn reconciliation_survives_foreground_process_group_interrupt() {
        surviving_reconcile("sigint");
    }

    #[test]
    fn reconciliation_survives_foreground_wait_timeout() {
        surviving_reconcile("timeout");
    }

    #[test]
    fn reconciliation_survives_foreground_wait_cancellation() {
        surviving_reconcile("cancel");
    }

    #[tokio::test]
    async fn cancellable_children_are_killed_when_the_wait_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new("/bin/sh");
        let pid_file = dir.path().join("pid");
        command
            .args(["-c", "echo $$ > \"$1\"; exec sleep 30", "sh"])
            .arg(&pid_file);
        let result = echo(dir.path())
            .run(command, Duration::from_millis(100), Child::Progress)
            .await
            .unwrap();
        assert!(result.is_none());
        let pid = fs::read_to_string(pid_file).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !std::process::Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cancellable child is still alive"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn a_cask_helper_requires_a_matching_homebrew_registration() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("brew");
        let app = dir.path().join("custom apps/Koidrive.app");
        let helper = app.join("Contents/Helpers/koi");
        executable(&helper, "#!/bin/sh\nexit 0\n");
        let prefixes = [prefix.clone()];
        let classify = || Install::resolve(&helper, "0.1.0", &prefixes, &[]).unwrap();
        assert_eq!(classify().method, Method::AppDownload);
        assert_eq!(classify().method.via(), Some(Via::App));
        assert_eq!(classify().command(), None);
        assert_eq!(
            Install::resolve(&helper, "0.1.0", &prefixes, std::slice::from_ref(&helper))
                .unwrap()
                .method,
            Method::App
        );
        let registration = prefix.join("Caskroom/koidrive/0.1.0/Koidrive.app");
        let other = dir.path().join("other/Koidrive.app");
        executable(&other.join("Contents/Helpers/koi"), "#!/bin/sh\nexit 0\n");
        fs::create_dir_all(registration.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&other, &registration).unwrap();
        assert_eq!(classify().method, Method::AppDownload);
        fs::remove_file(&registration).unwrap();
        std::os::unix::fs::symlink(&app, &registration).unwrap();
        let cask = classify();
        let brew = fs::canonicalize(&prefix).unwrap().join("bin/brew");
        assert_eq!(cask.method, Method::Cask { brew: brew.clone() });
        assert_eq!(
            Install::resolve(&helper, "0.1.0", &prefixes, std::slice::from_ref(&helper))
                .unwrap()
                .method,
            cask.method
        );
        assert_eq!(
            cask.command().as_deref(),
            Some([brew.to_str().unwrap(), "upgrade", "--cask", "koidrive"].as_slice())
        );
        let available = update::Update::Available {
            latest: "0.2.0".into(),
            url: "https://github.com/koidrive/koidrive/releases/tag/v0.2.0".into(),
            checked_at_ms: 1,
        };
        assert_eq!(
            cask.notice(&available, "0.1.0").command.as_deref(),
            Some(format!("{} upgrade --cask koidrive", brew.display()).as_str())
        );
        assert!(!prefix.join("bin/koi").exists());
    }

    #[test]
    fn cask_and_installer_aliases_resolve_without_changing_their_links() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("custombrew");
        let app = dir.path().join("custom apps/Koidrive.app");
        let helper = app.join("Contents/Helpers/koi");
        executable(&helper, "#!/bin/sh\nexit 0\n");
        let link = prefix.join("bin/koi");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&helper, &link).unwrap();
        assert_eq!(
            Install::resolve(&link, "0.1.0", &[], &[]).unwrap().method,
            Method::AppDownload
        );
        let registration = prefix.join("Caskroom/koidrive/0.1.0/Koidrive.app");
        fs::create_dir_all(registration.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&app, registration).unwrap();
        let cask = Install::resolve(&link, "0.1.0", &[], &[]).unwrap();
        assert_eq!(
            cask.method,
            Method::Cask {
                brew: fs::canonicalize(&prefix).unwrap().join("bin/brew")
            }
        );
        assert_eq!(cask.binary, fs::canonicalize(&helper).unwrap());
        let other_prefix = dir.path().join("otherbrew");
        let other_registration = other_prefix.join("Caskroom/koidrive/0.1.0/Koidrive.app");
        fs::create_dir_all(other_registration.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&app, other_registration).unwrap();
        assert_eq!(
            Install::resolve(&link, "0.1.0", &[other_prefix], &[])
                .unwrap()
                .method,
            cask.method,
        );
        let script = dir.path().join("standalone/koi");
        executable(&script, "#!/bin/sh\nexit 0\n");
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink("standalone/koi", &alias).unwrap();
        let second = dir.path().join("second");
        std::os::unix::fs::symlink("alias", &second).unwrap();
        let install = Install::resolve(&second, "0.1.0", &[], &[]).unwrap();
        assert_eq!(install.method, Method::Installer);
        assert_eq!(
            install.binary.parent(),
            fs::canonicalize(script.parent().unwrap()).ok().as_deref()
        );
        assert_eq!(fs::read_link(second).unwrap(), Path::new("alias"));
        assert_eq!(fs::read_link(link).unwrap(), helper);
    }

    #[test]
    fn unresolved_or_unrecognized_binaries_are_never_installer_targets() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing/koi");
        assert!(Install::resolve(&missing, "0.1.0", &[], &[]).is_err());
        let cycle = dir.path().join("koi");
        std::os::unix::fs::symlink("koi", &cycle).unwrap();
        assert!(Install::resolve(&cycle, "0.1.0", &[], &[]).is_err());
        for relative in ["different-name", "Other.app/Contents/Helpers/koi"] {
            let binary = dir.path().join(relative);
            executable(&binary, "#!/bin/sh\nexit 0\n");
            assert!(Install::resolve(&binary, "0.1.0", &[], &[]).is_err());
        }
    }

    async fn serve_once(listener: TcpListener, body: &'static str) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while !request.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn the_installer_runs_install_sh_into_the_kois_directory_for_the_release() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("my bin");
        fs::create_dir_all(&bin).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://{}/install.sh",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let served = tokio::spawn(serve_once(
            listener,
            "printf '%s|%s\\n' \"$KOIDRIVE_INSTALL_DIR\" \"$KOIDRIVE_VERSION\" > \"$KOIDRIVE_INSTALL_DIR/installed\"\necho 'Installed koi'\n",
        ));
        let version = Version::parse("0.1.2").unwrap();
        installer((url, false), &bin, &version, &mut echo(dir.path()))
            .await
            .unwrap();
        served.await.unwrap();
        assert_eq!(
            fs::read_to_string(bin.join("installed")).unwrap(),
            format!("{}|v0.1.2\n", bin.display())
        );
        let log = fs::read_to_string(dir.path().join("logs/update.log")).unwrap();
        assert!(log.contains("Installed koi\n"), "{log}");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://{}/install.sh",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let served = tokio::spawn(serve_once(
            listener,
            "echo 'checksum mismatch' >&2\nexit 3\n",
        ));
        let error = installer((url, false), &bin, &version, &mut echo(dir.path()))
            .await
            .unwrap_err();
        served.await.unwrap();
        assert_eq!(error.message, "install.sh failed (exit status 3)");
    }

    #[test]
    fn development_daemons_do_not_hand_the_service_to_installed_releases() {
        let dir = tempfile::tempdir().unwrap();
        let koi = dir.path().join("koi");
        executable(
            &koi,
            "#!/bin/sh\necho probe >> \"$0.calls\"\necho 'koi 99.0.0'\n",
        );
        for version in ["0.0.0-dev", "0.0.0-dev+abc123"] {
            let mut seen = HashMap::new();
            assert_eq!(
                scan(
                    &mut seen,
                    std::slice::from_ref(&koi),
                    &Version::parse(version).unwrap()
                ),
                None
            );
            assert!(seen.is_empty());
            assert!(!koi.with_extension("calls").exists());
        }
    }

    #[test]
    fn the_daemon_hands_the_service_to_the_newest_koi_once_per_change() {
        let dir = tempfile::tempdir().unwrap();
        let koi = |name: &str, version: &str| {
            let path = dir.path().join(name).join("koi");
            executable(&path, &format!("#!/bin/sh\necho 'koi {version}'\n"));
            path
        };
        let brew = koi("brew", "0.1.2");
        let app = koi("app", "0.1.3");
        let script = koi("script", "0.1.0");
        let missing = dir.path().join("gone/koi");
        let candidates = [brew.clone(), app.clone(), script.clone(), missing];
        let runs = Version::parse("0.1.0").unwrap();
        let mut seen = HashMap::new();

        assert_eq!(scan(&mut seen, &candidates, &runs), Some(app.clone()));
        assert_eq!(
            scan(&mut seen, &candidates, &runs),
            Some(app.clone()),
            "retry the handoff until the daemon changes"
        );

        let later = |seconds| SystemTime::now() + Duration::from_secs(seconds);
        executable(&brew, "#!/bin/sh\necho 'koi 0.2.0'\n");
        File::open(&brew).unwrap().set_modified(later(60)).unwrap();
        assert_eq!(scan(&mut seen, &candidates, &runs), Some(brew.clone()));

        executable(&script, "#!/bin/sh\necho 'koi 0.0.9'\n");
        File::open(&script)
            .unwrap()
            .set_modified(later(120))
            .unwrap();
        assert_eq!(
            scan(&mut seen, &candidates, &runs),
            Some(brew.clone()),
            "an older koi does not hide the cached newer install"
        );
        assert_eq!(
            scan(
                &mut HashMap::new(),
                &candidates,
                &Version::parse("0.3.0").unwrap()
            ),
            None
        );
    }

    #[test]
    fn handoff_retries_cached_versions_without_running_unchanged_binaries_again() {
        let dir = tempfile::tempdir().unwrap();
        let koi = dir.path().join("koi");
        executable(
            &koi,
            "#!/bin/sh\necho probe >> \"$0.calls\"\necho 'koi 0.2.0'\n",
        );
        let mut seen = HashMap::new();
        let candidates = [koi.clone()];
        for _ in 0..3 {
            assert_eq!(
                scan(&mut seen, &candidates, &Version::parse("0.1.0").unwrap()),
                Some(koi.clone())
            );
        }
        assert_eq!(
            scan(&mut seen, &candidates, &Version::parse("0.2.0").unwrap()),
            None
        );
        assert_eq!(
            fs::read_to_string(koi.with_extension("calls")).unwrap(),
            "probe\n"
        );
    }

    #[test]
    fn homebrew_without_an_environment_prefix_is_found_from_the_running_koi() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("custom");
        let keg = prefix.join("Cellar/koidrive/0.1.0/bin/koi");
        executable(&keg, "#!/bin/sh\nexit 0\n");
        let opt = prefix.join("opt/koidrive");
        fs::create_dir_all(opt.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(keg.ancestors().nth(2).unwrap(), &opt).unwrap();
        let link = prefix.join("bin/koi");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("../opt/koidrive/bin/koi", &link).unwrap();
        for binary in [&keg, &opt.join("bin/koi"), &link] {
            let install = Install::resolve(binary, "0.1.0", &[], &[]).unwrap();
            let brew = fs::canonicalize(&prefix).unwrap().join("bin/brew");
            assert_eq!(install.method, Method::Formula { brew: brew.clone() });
            assert_eq!(
                install.command().as_deref(),
                Some([brew.to_str().unwrap(), "upgrade", "koidrive"].as_slice())
            );
        }
        fs::remove_file(keg).unwrap();
        assert!(Install::resolve(&link, "0.1.0", &[], &[]).is_err());
    }

    #[test]
    fn another_install_never_changes_the_running_binarys_installation_method() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("brew");
        executable(&prefix.join("opt/koidrive/bin/koi"), "#!/bin/sh\nexit 0\n");
        let script = dir.path().join("standalone/koi");
        executable(&script, "#!/bin/sh\nexit 0\n");
        let install = Install::resolve(&script, "0.1.0", &[prefix], &[]).unwrap();
        assert_eq!(install.method, Method::Installer);
        assert_eq!(install.binary, fs::canonicalize(script).unwrap());
    }

    #[test]
    fn formula_ownership_survives_a_linked_cellar_and_invocation_alias() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("brew");
        let version = dir.path().join("external store/koidrive/0.1.0");
        let binary = version.join("bin/koi");
        executable(&binary, "#!/bin/sh\nexit 0\n");
        let opt = prefix.join("opt/koidrive");
        fs::create_dir_all(opt.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&version, &opt).unwrap();
        let link = prefix.join("bin/koi");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("../opt/koidrive/bin/koi", &link).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&link, &alias).unwrap();
        for invoked in [&alias, &link, &opt.join("bin/koi")] {
            let install = Install::resolve(invoked, "0.1.0", &[], &[]).unwrap();
            assert_eq!(
                install.method,
                Method::Formula {
                    brew: fs::canonicalize(&prefix).unwrap().join("bin/brew")
                }
            );
            assert_eq!(install.binary, fs::canonicalize(&binary).unwrap());
        }
    }

    #[test]
    fn invocation_hints_must_resolve_to_the_running_binary() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("actual/koi");
        executable(&binary, "#!/bin/sh\nexit 0\n");
        let kernel = fs::canonicalize(&binary).unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let alias = bin.join("koi");
        std::os::unix::fs::symlink(&binary, &alias).unwrap();
        assert_eq!(
            Install::invoked(&kernel, alias.as_os_str(), None),
            Some(alias.clone())
        );
        assert_eq!(
            Install::invoked(&kernel, OsStr::new("koi"), Some(bin.as_os_str())),
            Some(alias)
        );
        let other = dir.path().join("other/koi");
        executable(&other, "#!/bin/sh\nexit 0\n");
        assert_eq!(Install::invoked(&kernel, other.as_os_str(), None), None);
        let shadowed = std::env::join_paths([other.parent().unwrap(), &bin]).unwrap();
        assert_eq!(
            Install::invoked(&kernel, OsStr::new("koi"), Some(&shadowed)),
            None
        );
        assert_eq!(
            Install::invoked(&kernel, OsStr::new(""), Some(&shadowed)),
            None
        );
        let cwd = std::env::current_dir().unwrap();
        let relative_dir = tempfile::tempdir_in(&cwd).unwrap();
        let binary = relative_dir.path().join("koi");
        executable(&binary, "#!/bin/sh\nexit 0\n");
        let relative = Path::new(".").join(binary.strip_prefix(&cwd).unwrap());
        assert_eq!(
            Install::invoked(&binary, relative.as_os_str(), None),
            Some(relative)
        );
    }

    #[test]
    fn a_homebrew_receipt_blocks_installer_fallback_without_an_owning_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let keg = dir.path().join("external/package/0.1.0");
        let binary = keg.join("bin/koi");
        executable(&binary, "#!/bin/sh\nexit 0\n");
        fs::write(keg.join("INSTALL_RECEIPT.json"), "{}").unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&binary, &alias).unwrap();
        for invoked in [&binary, &alias] {
            let error = Install::resolve(invoked, "0.1.0", &[], &[]).err().unwrap();
            assert!(error.message.contains("Homebrew"), "{}", error.message);
        }
    }
}
