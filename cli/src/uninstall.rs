// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use koidrive_driver::{Credential, Flock, Paths};
use koidrive_ipc::{Request, Response};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::CliError;
use crate::output::Tone;
use crate::report::Done;
use crate::service::{Manager, ServiceManager, State};
use crate::setup::shown_folder;
use crate::terminal::Terminal;
use crate::update::{Install, Method};
use crate::{ipc, printable, service};

const RECORD: &str = "uninstall.json";
const PATH_MARKERS: [&str; 2] = ["; : 'koidrive PATH'", "; true 'koidrive PATH'"];
const BASH_PRELUDE: &str = r#"if [ -r "$HOME/.bash_login" ]; then . "$HOME/.bash_login"; elif [ -r "$HOME/.profile" ]; then . "$HOME/.profile"; fi"#;
const FOLDER_RECORDS: [&str; 4] = ["space.json", "device.json", "device.json.tmp", "staging"];
const KOI_MARK: &[u8] = b"Koidrive sync client";
const STATUS: Duration = Duration::from_secs(10);
const HELPER: Duration = Duration::from_secs(20);
const HOMEBREW: Duration = Duration::from_secs(5 * 60);
const RELEASE: Duration = Duration::from_secs(15);

pub struct Options {
    pub yes: bool,
    pub keep_signed_in: bool,
    pub last_device: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UninstalledData {
    pub device: DeviceOutcome,
    pub removed: Vec<Removed>,
    pub kept: Vec<Kept>,
}

/// Whether this device is still on its server's device list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeviceOutcome {
    NotPaired,
    SignedOut {
        server: String,
    },
    /// Still on the server's list, because of `--keep-signed-in` or, with `problem`, because the sign-out did not go
    /// through.
    StillListed {
        #[serde(skip_serializing_if = "Option::is_none")]
        server: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        problem: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Removed {
    /// The background service, by its launchd label or systemd unit.
    Service {
        name: String,
    },
    App {
        path: String,
    },
    Command {
        path: String,
    },
    /// The Homebrew command that removed the cask or the formula.
    Homebrew {
        argv: Vec<String>,
    },
    Files {
        path: String,
    },
    /// A shell profile the installer's PATH line was removed from, or deleted when nothing else was in it.
    ShellProfile {
        path: String,
    },
    /// The `.koidrive` folder of a synced folder.
    FolderData {
        path: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kept {
    /// A folder this device synced, with every file in it.
    Folder {
        path: String,
    },
    /// A part of a synced folder's `.koidrive` that still holds files, such as the local trash.
    FolderData {
        path: String,
    },
    /// The `.koidrive` folder of a synced folder that another device or profile links.
    LinkedElsewhere {
        path: String,
    },
    DevelopmentBuild {
        path: String,
    },
    /// An app outside the Applications folders, such as one still on its disk image.
    App {
        path: String,
    },
    LoginItem {
        path: String,
        problem: String,
    },
    /// A path that holds or lies inside a synced folder, so it stays with the folder.
    InFolder {
        path: String,
        folder: String,
    },
    /// A settings or state folder that holds files Koidrive does not write.
    Unrecognized {
        path: String,
    },
    /// The file the removed `koi` command linked to.
    LinkTarget {
        path: String,
    },
    /// A program or file that another Koidrive profile on this computer still uses.
    Shared {
        path: String,
    },
    /// The installer's PATH line in a shell profile, which another Koidrive profile still uses.
    PathLine {
        path: String,
    },
    /// The background service of another Koidrive profile on this computer; `argv` removes that profile.
    OtherProfile {
        service: String,
        argv: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Owner {
    server: String,
    device_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    folders: Vec<PathBuf>,
    owner: Option<Owner>,
}

#[derive(Debug, PartialEq, Eq)]
enum Program {
    Command(PathBuf),
    App(PathBuf),
    Homebrew {
        brew: PathBuf,
        cask: bool,
        koi: PathBuf,
        app: Option<PathBuf>,
    },
    Elsewhere(PathBuf),
    Development(PathBuf),
}

struct Found {
    paired: bool,
    state: bool,
    owner: Option<Owner>,
    folders: Vec<PathBuf>,
    service: Option<(Box<dyn Manager>, service::Service)>,
    others: Vec<service::Installed>,
    programs: Vec<Program>,
    files: Vec<PathBuf>,
    app_files: Vec<PathBuf>,
    profiles: Vec<PathBuf>,
    kept: Vec<Kept>,
}

impl Found {
    fn is_empty(&self) -> bool {
        !self.paired
            && self.folders.is_empty()
            && self.service.is_none()
            && self.files.is_empty()
            && self.app_files.is_empty()
            && self.profiles.is_empty()
            && self
                .programs
                .iter()
                .all(|program| matches!(program, Program::Development(_) | Program::Elsewhere(_)))
    }
}

#[derive(Default)]
struct Run {
    removed: Vec<Removed>,
    kept: Vec<Kept>,
    problems: Vec<String>,
    next: Vec<Vec<String>>,
    folders: Vec<PathBuf>,
}

impl Run {
    fn allowed(&mut self, path: &Path) -> bool {
        if fs::symlink_metadata(path).is_err() {
            return true;
        }
        let root = fs::metadata(path).is_ok_and(|meta| meta.is_dir())
            && fs::symlink_metadata(path.join(".koidrive")).is_ok();
        let folder = (self.folders.iter())
            .find(|folder| holds(folder, path) || holds(path, folder))
            .cloned()
            .or_else(|| root.then(|| path.to_path_buf()));
        let Some(folder) = folder else {
            return true;
        };
        self.kept.push(Kept::InFolder {
            path: path.display().to_string(),
            folder: folder.display().to_string(),
        });
        false
    }

    fn remove(&mut self, path: &Path, removed: Removed) {
        if !self.allowed(path) {
            return;
        }
        match remove_path(path) {
            Ok(true) => self.removed.push(removed),
            Ok(false) => {}
            Err(error) => self
                .problems
                .push(format!("cannot remove {}: {error}", shown_folder(path))),
        }
    }

    fn remove_dir(&mut self, dir: &Path, ours: fn(&str) -> bool) {
        let shown = Removed::Files {
            path: dir.display().to_string(),
        };
        if dangling(dir) {
            return self.remove(dir, shown);
        }
        let target = fs::symlink_metadata(dir)
            .is_ok_and(|meta| meta.is_symlink())
            .then(|| fs::canonicalize(dir).ok())
            .flatten();
        let real = target.as_deref().unwrap_or(dir);
        if fs::symlink_metadata(real).is_err() || !self.allowed(dir) || !self.allowed(real) {
            return;
        }
        let mut gone = Vec::new();
        let emptied = fs::read_dir(real)
            .and_then(|entries| {
                let mut names = (entries.map(|entry| entry.map(|entry| entry.file_name())))
                    .collect::<io::Result<Vec<_>>>()?;
                names.sort_by_key(|name| name == RECORD);
                for name in names {
                    if name.to_str().is_some_and(ours) && remove_path(&real.join(&name))? {
                        gone.push(dir.join(name));
                    }
                }
                Ok(())
            })
            .and_then(|()| fs::remove_dir(real));
        match emptied {
            Ok(()) if target.is_some() => self.remove(dir, shown),
            Ok(()) => self.removed.push(shown),
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
                self.removed.extend(gone.iter().map(|path| Removed::Files {
                    path: path.display().to_string(),
                }));
                self.kept.push(Kept::Unrecognized {
                    path: dir.display().to_string(),
                })
            }
            Err(error) => self
                .problems
                .push(format!("cannot remove {}: {error}", shown_folder(dir))),
        }
    }
}

fn holds(outer: &Path, inner: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if inner.starts_with(outer) {
        return true;
    }
    let Ok(outer) = fs::metadata(outer) else {
        return false;
    };
    inner
        .ancestors()
        .filter_map(|dir| fs::metadata(dir).ok())
        .any(|dir| dir.dev() == outer.dev() && dir.ino() == outer.ino())
}

fn is_state_entry(name: &str) -> bool {
    let known = |name: &str| Paths::STATE_ENTRIES.contains(&name);
    known(name)
        || name.starts_with(".tmp")
        || name
            .strip_prefix('.')
            .and_then(|name| name.strip_suffix(".tmp"))
            .and_then(|name| name.rsplit_once('.'))
            .is_some_and(|(name, pid)| known(name) && pid.chars().all(|c| c.is_ascii_digit()))
}

fn is_state(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(".koidrive")).is_err()
        && fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry.file_type().is_ok_and(|kind| !kind.is_dir())
                    && entry.file_name().to_str().is_some_and(is_state_entry)
            })
        })
}

fn dangling(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink())
        && fs::metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

pub async fn run(paths: &Paths, options: Options, terminal: &Terminal) -> Result<Done, CliError> {
    if rustix::process::geteuid().is_root() {
        return Err(CliError::usage(
            "koi uninstall removes Koidrive for one user; run it as your own user, not as root",
        ));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or_else(|| CliError::usage("HOME is not set to an absolute path"))?;
    let found = find(paths, &home).await;
    if found.is_empty() {
        let mut kept = found.kept.clone();
        kept.extend(kept_programs(&found.programs));
        let text = format!(
            "Koidrive is not installed for this user, so there is nothing to remove.\n{}",
            kept_text(&kept)
        );
        let data = UninstalledData {
            device: DeviceOutcome::NotPaired,
            removed: Vec::new(),
            kept,
        };
        return Ok(Done::ok(data, text).unchanged());
    }
    confirm(&found, &options, terminal).await?;
    let shared = !found.others.is_empty();
    let mut run = Run {
        kept: found.kept.clone(),
        folders: found.folders.clone(),
        ..Run::default()
    };
    record(paths, &found, &mut run);
    if !shared {
        quit_apps(&found.programs, &mut run).await;
    }
    let device = sign_out(paths, &found, &options, terminal).await;
    remove_service(&found, &mut run).await;
    if let Err(problem) = released(paths).await {
        run.problems.push(problem);
        return finish(device, run);
    }
    for folder in &found.folders {
        clean_folder(folder, found.owner.as_ref(), &mut run);
    }
    if !run.problems.is_empty() {
        return finish(device, run);
    }
    for file in &found.files {
        if *file == paths.state_dir {
            run.remove_dir(file, is_state_entry);
        } else if Some(file.as_path()) == paths.config_file.parent() {
            run.remove_dir(file, |name| name == "config.toml");
        } else {
            run.remove(
                file,
                Removed::Files {
                    path: file.display().to_string(),
                },
            );
        }
    }
    prune_runtime(paths);
    if shared {
        keep_shared(&found, &mut run);
        return finish(device, run);
    }
    for file in &found.app_files {
        run.remove(
            file,
            Removed::Files {
                path: file.display().to_string(),
            },
        );
    }
    for profile in &found.profiles {
        match strip_path_line(profile) {
            Ok(true) => run.removed.push(Removed::ShellProfile {
                path: profile.display().to_string(),
            }),
            Ok(false) => {}
            Err(error) => run.problems.push(format!(
                "cannot remove the PATH line from {}: {error}",
                shown_folder(profile)
            )),
        }
    }
    if !run.problems.is_empty() {
        return finish(device, run);
    }
    remove_programs(&found.programs, &mut run).await;
    finish(device, run)
}

async fn find(paths: &Paths, home: &Path) -> Found {
    let recorded = read_record(paths);
    let credential = paths.credential();
    let paired = credential.exists();
    let owner = Credential::load(&credential)
        .ok()
        .flatten()
        .map(|credential| Owner {
            server: credential.server,
            device_id: credential.device_id,
        })
        .or(recorded.owner);
    let mut folders = recorded.folders;
    let state = is_state(&paths.state_dir);
    if state
        && let Ok(Ok(Response::Status { status })) =
            tokio::time::timeout(STATUS, ipc::run(paths, Request::Status)).await
    {
        for folder in status.folders {
            let folder = PathBuf::from(folder.path);
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
    }
    let manager = service::system();
    let this = service::this_service(paths).ok();
    let own = this.as_ref().map(|service| manager.name(service));
    let others = service::installed_services(manager.kind(), home)
        .into_iter()
        .filter(|installed| Some(&installed.name) != own.as_ref())
        .collect();
    let mut files = Vec::new();
    if cfg!(target_os = "macos")
        && let Some(service) = &this
    {
        files.push(service.log_file());
    }
    let service = this.filter(|service| match manager.state(service) {
        Ok(state) => state != State::NotInstalled,
        Err(_) => service.file(manager.kind()).exists(),
    });
    let mut also = Vec::new();
    if let Some(service) = &service
        && let Some(binary) = manager.installed(service)
    {
        also.push(PathBuf::from(binary));
    }
    let programs = programs(home, &also);
    let mut kept = Vec::new();
    match state || dangling(&paths.state_dir) {
        true => files.push(paths.state_dir.clone()),
        false if fs::symlink_metadata(&paths.state_dir).is_ok() => kept.push(Kept::Unrecognized {
            path: paths.state_dir.display().to_string(),
        }),
        false => {}
    }
    files.extend(paths.config_file.parent().map(Path::to_path_buf));
    if paths.runtime_dir != paths.state_dir {
        files.push(paths.socket());
    }
    let mut app_files = Vec::new();
    if cfg!(target_os = "macos") {
        app_files.extend(
            [
                "Library/Logs/Koidrive",
                "Library/Application Support/Koidrive",
                "Library/Preferences/dev.koidrive.Koidrive.plist",
                "Library/Caches/dev.koidrive.Koidrive",
                "Library/Caches/dev.koidrive.app-install",
                "Library/HTTPStorages/dev.koidrive.Koidrive",
                "Library/Saved Application State/dev.koidrive.Koidrive.savedState",
            ]
            .iter()
            .map(|path| home.join(path)),
        );
        app_files.extend(
            application_folders(home).map(|folder| folder.join(".koidrive-app-install.lock")),
        );
    }
    files.retain(|path| fs::symlink_metadata(path).is_ok());
    app_files.retain(|path| fs::symlink_metadata(path).is_ok());
    Found {
        paired,
        state,
        owner,
        folders,
        service: service.map(|service| (manager, service)),
        others,
        programs,
        files,
        app_files,
        profiles: profiles(home)
            .into_iter()
            .filter(|path| has_path_line(path))
            .collect(),
        kept,
    }
}

fn application_folders(home: &Path) -> impl Iterator<Item = PathBuf> {
    #[cfg(feature = "test-install-inventory")]
    if let Some(root) = std::env::var_os("KOIDRIVE_TEST_INSTALL_INVENTORY") {
        return vec![PathBuf::from(root).join("Applications")].into_iter();
    }
    vec![PathBuf::from("/Applications"), home.join("Applications")].into_iter()
}

fn programs(home: &Path, also: &[PathBuf]) -> Vec<Program> {
    let mut programs = Vec::new();
    for install in Install::every(also) {
        let binary = install.binary().to_path_buf();
        let bundle = || binary.ancestors().nth(3).map(Path::to_path_buf);
        let program = match install.method() {
            Method::Development => Program::Development(binary.clone()),
            Method::AppDownload => Program::Elsewhere(bundle().unwrap_or_else(|| binary.clone())),
            _ if !is_koi(&binary) => continue,
            Method::Installer => {
                for location in command_locations(home, also, &binary) {
                    let program = Program::Command(location);
                    if !programs.contains(&program) {
                        programs.push(program);
                    }
                }
                continue;
            }
            Method::App => match bundle() {
                Some(bundle) => Program::App(bundle),
                None => continue,
            },
            Method::Cask { brew } => Program::Homebrew {
                brew: brew.clone(),
                cask: true,
                koi: binary.clone(),
                app: bundle(),
            },
            Method::Formula { brew } => Program::Homebrew {
                brew: brew.clone(),
                cask: false,
                koi: binary.clone(),
                app: None,
            },
        };
        if !programs.contains(&program) {
            programs.push(program);
        }
    }
    programs
}

fn command_locations(home: &Path, also: &[PathBuf], binary: &Path) -> Vec<PathBuf> {
    let invoked = std::env::args_os()
        .next()
        .map(PathBuf::from)
        .filter(|arg| arg.components().count() > 1)
        .and_then(|arg| std::path::absolute(arg).ok());
    let on_path = crate::update::on_path(OsStr::new("koi"), std::env::var_os("PATH").as_deref());
    let mut locations: Vec<PathBuf> = Vec::new();
    for location in std::iter::once(home.join(".local/bin/koi"))
        .chain(invoked)
        .chain(on_path)
        .chain(also.iter().cloned())
    {
        if fs::canonicalize(&location).is_ok_and(|resolved| resolved == binary)
            && !locations.contains(&location)
        {
            locations.push(location);
        }
    }
    if locations.is_empty() {
        locations.push(binary.to_path_buf());
    }
    locations
}

fn is_koi(binary: &Path) -> bool {
    fs::read(binary).is_ok_and(|bytes| {
        bytes
            .windows(KOI_MARK.len())
            .any(|window| window == KOI_MARK)
    })
}

fn profiles(home: &Path) -> Vec<PathBuf> {
    let absolute = |name| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let mut profiles: Vec<PathBuf> = absolute("ZDOTDIR")
        .map(|dir| dir.join(".zprofile"))
        .into_iter()
        .collect();
    profiles.push(home.join(".zprofile"));
    profiles.push(home.join(".bash_profile"));
    profiles.push(
        absolute("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("fish/conf.d/koidrive.fish"),
    );
    profiles.dedup();
    profiles
}

fn is_path_line(line: &str) -> bool {
    PATH_MARKERS.iter().any(|marker| line.ends_with(marker))
}

fn has_path_line(profile: &Path) -> bool {
    fs::read_to_string(profile).is_ok_and(|text| text.lines().any(is_path_line))
}

fn kept_programs(programs: &[Program]) -> Vec<Kept> {
    programs
        .iter()
        .filter_map(|program| match program {
            Program::Development(path) => Some(Kept::DevelopmentBuild {
                path: path.display().to_string(),
            }),
            Program::Elsewhere(path) => Some(Kept::App {
                path: path.display().to_string(),
            }),
            _ => None,
        })
        .collect()
}

async fn confirm(found: &Found, options: &Options, terminal: &Terminal) -> Result<(), CliError> {
    if options.yes {
        return Ok(());
    }
    if !terminal.guided() {
        return Err(CliError::usage(
            "koi uninstall asks before it removes anything; run it on a terminal, or add --yes",
        )
        .then(&["koi", "uninstall", "--yes"]));
    }
    for line in plan(found, options) {
        terminal.message(&line);
    }
    match terminal
        .confirm("Remove Koidrive from this computer?", false)
        .await?
    {
        true => Ok(()),
        false => Err("stopped; nothing was changed".into()),
    }
}

fn plan(found: &Found, options: &Options) -> Vec<String> {
    let mut lines = vec!["This removes:".to_owned()];
    if found.paired && !options.keep_signed_in {
        lines.push(match &found.owner {
            Some(owner) => format!("  this device from {}", printable(&owner.server)),
            None => "  this device from its server".to_owned(),
        });
    }
    if let Some((manager, service)) = &found.service {
        lines.push(format!(
            "  the background service {}",
            manager.name(service)
        ));
    }
    let shared = !found.others.is_empty();
    for program in found.programs.iter().filter(|_| !shared) {
        match program {
            Program::Command(path) => lines.push(format!("  {}", shown_folder(path))),
            Program::App(path) => lines.push(format!("  the app {}", shown_folder(path))),
            Program::Homebrew { cask, .. } => lines.push(format!(
                "  the Homebrew {} koidrive",
                if *cask { "cask" } else { "formula" }
            )),
            Program::Elsewhere(_) | Program::Development(_) => {}
        }
    }
    for path in found
        .files
        .iter()
        .chain(found.app_files.iter().filter(|_| !shared))
    {
        lines.push(format!("  {}", shown_folder(path)));
    }
    for path in found.profiles.iter().filter(|_| !shared) {
        lines.push(format!("  the PATH line in {}", shown_folder(path)));
    }
    for folder in &found.folders {
        lines.push(format!("  {}", shown_folder(&folder.join(".koidrive"))));
    }
    if !found.folders.is_empty() {
        lines.push("It keeps your files:".to_owned());
        for folder in &found.folders {
            lines.push(format!("  {}", shown_folder(folder)));
        }
    }
    if shared {
        lines.push(
            "It keeps the koi command and the app, which another Koidrive profile uses:".to_owned(),
        );
        for other in &found.others {
            lines.push(format!("  the background service {}", other.name));
        }
    }
    for kept in &found.kept {
        if let Kept::Unrecognized { path } = kept {
            lines.push(format!(
                "It keeps {}, which holds files Koidrive does not write.",
                shown_folder(Path::new(path))
            ));
        }
    }
    lines
}

fn read_record(paths: &Paths) -> Record {
    fs::read(paths.state_dir.join(RECORD))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn record(paths: &Paths, found: &Found, run: &mut Run) {
    if !found.state || !paths.state_dir.is_dir() {
        return;
    }
    let record = Record {
        folders: found.folders.clone(),
        owner: found.owner.clone(),
    };
    let path = paths.state_dir.join(RECORD);
    let written = tempfile::NamedTempFile::new_in(&paths.state_dir).and_then(|mut file| {
        file.write_all(&serde_json::to_vec(&record).expect("the record serializes"))?;
        file.as_file().sync_all()?;
        file.persist(&path).map(drop).map_err(|error| error.error)
    });
    if let Err(error) = written {
        run.problems
            .push(format!("cannot write {}: {error}", shown_folder(&path)));
    }
}

async fn sign_out(
    paths: &Paths,
    found: &Found,
    options: &Options,
    terminal: &Terminal,
) -> DeviceOutcome {
    let still_listed = |problem: Option<String>| DeviceOutcome::StillListed {
        server: found.owner.as_ref().map(|owner| owner.server.clone()),
        device_id: found.owner.as_ref().map(|owner| owner.device_id.clone()),
        problem,
    };
    if !found.paired {
        return DeviceOutcome::NotPaired;
    }
    if options.keep_signed_in {
        return still_listed(None);
    }
    let leave = |confirm_last_device| {
        ipc::run(
            paths,
            Request::Leave {
                confirm_last_device,
            },
        )
    };
    let mut answer = leave(options.last_device).await;
    if let Err(error) = &answer
        && !options.last_device
        && !options.yes
        && terminal.guided()
        && error
            .next
            .iter()
            .any(|next| next.argv.iter().any(|arg| arg == "--last-device"))
    {
        terminal.message(&printable(&error.message));
        if terminal
            .confirm("Sign this device out anyway?", false)
            .await
            .unwrap_or(false)
        {
            answer = leave(true).await;
        }
    }
    match answer {
        Ok(Response::SignedOut { .. }) => match &found.owner {
            Some(owner) => DeviceOutcome::SignedOut {
                server: owner.server.clone(),
            },
            None => still_listed(Some("the server's address is unknown".into())),
        },
        Ok(other) => still_listed(Some(format!("the daemon answered {other:?}"))),
        Err(error) => still_listed(Some(error.message)),
    }
}

async fn remove_service(found: &Found, run: &mut Run) {
    let Some((manager, service)) = &found.service else {
        return;
    };
    let name = manager.name(service);
    let removed = match manager.state(service) {
        Ok(State::NotInstalled) => Ok(()),
        Ok(_) => manager.remove(service),
        Err(problem) if manager.kind() == ServiceManager::Systemd => {
            remove_path(&service.file(manager.kind()))
                .map(drop)
                .map_err(|error| format!("{problem}; {error}"))
        }
        Err(problem) => Err(problem),
    };
    let mut removed = removed;
    let deadline = Instant::now() + RELEASE;
    while removed.is_ok() {
        match manager.state(service) {
            Ok(State::NotInstalled) => break,
            Err(_) if !service.file(manager.kind()).exists() => break,
            Ok(_) if Instant::now() >= deadline => {
                removed = Err("it is still loaded after removal".to_owned());
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            Err(problem) => removed = Err(problem),
        }
    }
    match removed {
        Ok(()) => run.removed.push(Removed::Service { name }),
        Err(problem) => run.problems.push(format!(
            "cannot remove the background service {name}: {problem}"
        )),
    }
}

async fn released(paths: &Paths) -> Result<(), String> {
    let lock = paths.lock();
    let deadline = Instant::now() + RELEASE;
    loop {
        let file = match fs::File::open(&lock) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("cannot open {}: {error}", shown_folder(&lock))),
            Ok(file) => file,
        };
        match Flock::take(file) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(None) => {
                return Err(
                    "a koi daemon still uses this device's profile, so its state stays; stop it"
                        .into(),
                );
            }
            Err(error) => return Err(format!("cannot lock {}: {error}", shown_folder(&lock))),
        }
    }
}

fn clean_folder(root: &Path, owner: Option<&Owner>, run: &mut Run) {
    if fs::symlink_metadata(root).is_ok_and(|meta| meta.is_dir()) {
        run.kept.push(Kept::Folder {
            path: root.display().to_string(),
        });
    }
    let private = root.join(".koidrive");
    match folder_data(&private, owner) {
        Ok(Cleaned::Nothing) => {}
        Ok(Cleaned::Removed) => run.removed.push(Removed::FolderData {
            path: private.display().to_string(),
        }),
        Ok(Cleaned::LinkedElsewhere) => run.kept.push(Kept::LinkedElsewhere {
            path: private.display().to_string(),
        }),
        Ok(Cleaned::Kept(paths)) => {
            run.kept
                .extend(paths.into_iter().map(|path| Kept::FolderData {
                    path: path.display().to_string(),
                }))
        }
        Err(error) => run.problems.push(format!(
            "cannot clean up {}: {error}",
            shown_folder(&private)
        )),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Cleaned {
    Nothing,
    Removed,
    LinkedElsewhere,
    Kept(Vec<PathBuf>),
}

fn folder_data(private: &Path, owner: Option<&Owner>) -> io::Result<Cleaned> {
    match fs::symlink_metadata(private) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Cleaned::Nothing),
        Err(error) => return Err(error),
        Ok(meta) if !meta.is_dir() => return Ok(Cleaned::Kept(vec![private.to_path_buf()])),
        Ok(_) => {}
    }
    match fs::read(private.join("device.json")) {
        Ok(bytes) => {
            let linked = serde_json::from_slice::<Owner>(&bytes).ok();
            if linked.is_none() || linked.as_ref() != owner {
                return Ok(Cleaned::LinkedElsewhere);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for name in FOLDER_RECORDS {
        remove_path(&private.join(name))?;
    }
    let trash = private.join("trash");
    if fs::symlink_metadata(&trash).is_err() || prune_empty(&trash)? {
        for index in ["trash.json", "trash.json.tmp"] {
            remove_path(&private.join(index))?;
        }
    }
    let mut kept = Vec::new();
    for entry in fs::read_dir(private)? {
        let path = entry?.path();
        if !prune_empty(&path)? {
            kept.push(path);
        }
    }
    kept.sort();
    if !kept.is_empty() {
        return Ok(Cleaned::Kept(kept));
    }
    fs::remove_dir(private)?;
    Ok(Cleaned::Removed)
}

fn prune_empty(path: &Path) -> io::Result<bool> {
    if !fs::symlink_metadata(path)?.is_dir() {
        return Ok(false);
    }
    let mut empty = true;
    for entry in fs::read_dir(path)? {
        empty &= prune_empty(&entry?.path())?;
    }
    if empty {
        fs::remove_dir(path)?;
    }
    Ok(empty)
}

fn remove_path(path: &Path) -> io::Result<bool> {
    let removed = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    };
    match removed {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        other => other.map(|()| true),
    }
}

fn prune_runtime(paths: &Paths) {
    if paths.runtime_dir != paths.state_dir {
        let _ = fs::remove_dir(&paths.runtime_dir);
    }
}

fn strip_path_line(profile: &Path) -> io::Result<bool> {
    let text = match fs::read_to_string(profile) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        other => other?,
    };
    let rest: Vec<&str> = text.lines().filter(|line| !is_path_line(line)).collect();
    if rest.len() == text.lines().count() {
        return Ok(false);
    }
    let rest = rest.join("\n");
    let created = rest.trim().is_empty()
        || (profile.file_name() == Some(OsStr::new(".bash_profile"))
            && rest.trim() == BASH_PRELUDE);
    if created && !fs::symlink_metadata(profile)?.is_symlink() {
        fs::remove_file(profile)?;
        return Ok(true);
    }
    let target = fs::canonicalize(profile)?;
    let dir = target.parent().unwrap_or(Path::new("/"));
    let mut staged = tempfile::NamedTempFile::new_in(dir)?;
    staged.write_all(rest.as_bytes())?;
    if !rest.is_empty() && text.ends_with('\n') {
        staged.write_all(b"\n")?;
    }
    staged
        .as_file()
        .set_permissions(fs::metadata(&target)?.permissions())?;
    staged.as_file().sync_all()?;
    staged.persist(&target).map_err(|error| error.error)?;
    Ok(true)
}

async fn helper(program: &Path, args: &[&str], within: Duration) -> Result<(), String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(within, output).await {
        Err(_) => Err(format!(
            "{} did not finish within {} seconds",
            program.display(),
            within.as_secs()
        )),
        Ok(Err(error)) => Err(format!("cannot run {}: {error}", program.display())),
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => Err(format!(
            "{} {} failed ({}): {}",
            program.display(),
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

fn bundles(programs: &[Program]) -> Vec<&Path> {
    programs
        .iter()
        .filter_map(|program| match program {
            Program::App(bundle) | Program::Elsewhere(bundle) => Some(bundle.as_path()),
            Program::Homebrew {
                app: Some(bundle), ..
            } => Some(bundle.as_path()),
            _ => None,
        })
        .collect()
}

async fn quit_apps(programs: &[Program], run: &mut Run) {
    let bundles = bundles(programs);
    if bundles.is_empty() || !cfg!(target_os = "macos") {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let paths: Vec<String> = bundles
            .iter()
            .map(|bundle| bundle.display().to_string())
            .collect();
        let mut args = vec!["-l", "JavaScript", "-e", crate::app_install::QUIESCE];
        args.extend(paths.iter().map(String::as_str));
        if let Err(problem) = helper(Path::new("/usr/bin/osascript"), &args, HELPER).await {
            run.problems
                .push(format!("cannot quit the Koidrive app: {problem}"));
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = run;
}

async fn unregister_login_item(bundle: &Path, run: &mut Run) {
    let app = bundle.join("Contents/MacOS/Koidrive");
    if !app.is_file() {
        return;
    }
    if let Err(problem) = helper(&app, &["--unregister-login-item"], HELPER).await {
        run.kept.push(Kept::LoginItem {
            path: bundle.display().to_string(),
            problem,
        });
    }
}

async fn remove_programs(programs: &[Program], run: &mut Run) {
    for bundle in bundles(programs) {
        unregister_login_item(bundle, run).await;
    }
    run.kept.extend(kept_programs(programs));
    for program in programs {
        match program {
            Program::Command(path) => {
                let target = fs::symlink_metadata(path)
                    .is_ok_and(|meta| meta.is_symlink())
                    .then(|| fs::canonicalize(path).ok())
                    .flatten();
                run.remove(
                    path,
                    Removed::Command {
                        path: path.display().to_string(),
                    },
                );
                if let Some(target) = target
                    && !programs.contains(&Program::Command(target.clone()))
                    && fs::symlink_metadata(&target).is_ok()
                {
                    run.kept.push(Kept::LinkTarget {
                        path: target.display().to_string(),
                    });
                }
            }
            Program::App(path) => run.remove(
                path,
                Removed::App {
                    path: path.display().to_string(),
                },
            ),
            Program::Homebrew { brew, cask, .. } => {
                let argv = vec![
                    brew_word(brew),
                    "uninstall".to_owned(),
                    if *cask { "--cask" } else { "--formula" }.to_owned(),
                    "koidrive".to_owned(),
                ];
                let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
                match helper(brew, &args, HOMEBREW).await {
                    Ok(()) => run.removed.push(Removed::Homebrew { argv }),
                    Err(problem) => {
                        run.problems.push(problem);
                        run.next.push(argv);
                    }
                }
            }
            Program::Elsewhere(_) | Program::Development(_) => {}
        }
    }
}

fn keep_shared(found: &Found, run: &mut Run) {
    let shared = |path: &Path| Kept::Shared {
        path: path.display().to_string(),
    };
    for program in &found.programs {
        run.kept.push(match program {
            Program::Command(path) | Program::App(path) => shared(path),
            Program::Homebrew { koi, app, .. } => shared(app.as_deref().unwrap_or(koi)),
            Program::Elsewhere(_) | Program::Development(_) => continue,
        });
    }
    run.kept.extend(kept_programs(&found.programs));
    run.kept
        .extend(found.app_files.iter().map(|path| shared(path)));
    run.kept
        .extend(found.profiles.iter().map(|path| Kept::PathLine {
            path: path.display().to_string(),
        }));
    let on_path = crate::update::on_path(OsStr::new("koi"), std::env::var_os("PATH").as_deref())
        .and_then(|koi| fs::canonicalize(koi).ok());
    for other in &found.others {
        let binary = (other.binary.as_deref())
            .map(PathBuf::from)
            .into_iter()
            .chain(found.programs.iter().filter_map(|program| match program {
                Program::Command(path) => Some(path.clone()),
                Program::App(bundle) => Some(bundle.join("Contents/Helpers/koi")),
                Program::Homebrew { koi, .. } => Some(koi.clone()),
                Program::Elsewhere(_) | Program::Development(_) => None,
            }))
            .find(|binary| is_koi(binary))
            .or_else(|| std::env::current_exe().ok());
        let koi = match binary {
            Some(binary) if on_path.is_none() || fs::canonicalize(&binary).ok() != on_path => {
                binary.display().to_string()
            }
            _ => "koi".to_owned(),
        };
        let mut argv = vec!["env".to_owned()];
        for (name, value) in &other.env {
            if value.is_none() {
                argv.extend(["-u".to_owned(), (*name).to_owned()]);
            }
        }
        for (name, value) in &other.env {
            if let Some(value) = value {
                argv.push(format!("{name}={value}"));
            }
        }
        argv.extend([koi, "uninstall".to_owned()]);
        run.next.push(argv.clone());
        run.kept.push(Kept::OtherProfile {
            service: other.name.clone(),
            argv,
        });
    }
}

fn brew_word(brew: &Path) -> String {
    let selected = crate::update::on_path(OsStr::new("brew"), std::env::var_os("PATH").as_deref())
        .and_then(|selected| fs::canonicalize(selected).ok());
    match selected.is_some() && selected == fs::canonicalize(brew).ok() {
        true => "brew".to_owned(),
        false => brew.display().to_string(),
    }
}

fn finish(device: DeviceOutcome, run: Run) -> Result<Done, CliError> {
    let Run {
        removed,
        kept,
        problems,
        next,
        ..
    } = run;
    let data = UninstalledData {
        device,
        removed,
        kept,
    };
    let other = (data.kept.iter()).any(|kept| matches!(kept, Kept::OtherProfile { .. }));
    let header = match (other, data.removed.is_empty()) {
        (false, _) => {
            "Removed Koidrive from this computer.
"
        }
        (true, false) => {
            "Removed this Koidrive profile from this computer.
"
        }
        (true, true) => {
            "Nothing of this Koidrive profile is left to remove.
"
        }
    };
    let text = text(&data);
    let mut done = Done::ok(data, text);
    for argv in &next {
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        done = done.then(&argv);
    }
    if problems.is_empty() {
        done.text.insert_str(0, header);
        done.tone = Tone::Completed;
        return Ok(done);
    }
    let message = match next.is_empty() {
        true => format!("{}; run koi uninstall again to finish", problems.join("; ")),
        false => problems.join("; "),
    };
    Err(CliError {
        next: done.next.clone(),
        partial: Some(Box::new(done)),
        ..CliError::new(koidrive_ipc::Code::Failed, message)
    })
}

fn text(data: &UninstalledData) -> String {
    let mut lines = Vec::new();
    match &data.device {
        DeviceOutcome::NotPaired => {}
        DeviceOutcome::SignedOut { server } => {
            lines.push(format!("Signed this device out of {}.", printable(server)));
        }
        DeviceOutcome::StillListed {
            server,
            device_id,
            problem,
        } => {
            let server = server
                .as_deref()
                .map_or_else(|| "Its server".to_owned(), printable);
            if let Some(problem) = problem {
                lines.push(format!(
                    "Could not sign this device out: {}.",
                    printable(problem)
                ));
            }
            lines.push(match device_id {
                Some(id) => format!(
                    "{server} still lists this device. To remove it, run this on another device: koi device revoke {id}"
                ),
                None => format!(
                    "{server} still lists this device. To remove it, revoke it from another device with koi device revoke."
                ),
            });
        }
    }
    for removed in &data.removed {
        let shown = |path: &str| shown_folder(Path::new(path));
        lines.push(match removed {
            Removed::Service { name } => format!("Removed the background service {name}."),
            Removed::App { path } => format!("Removed the app {}.", shown(path)),
            Removed::Command { path } | Removed::Files { path } | Removed::FolderData { path } => {
                format!("Removed {}.", shown(path))
            }
            Removed::Homebrew { argv } => format!("Ran {}.", argv.join(" ")),
            Removed::ShellProfile { path } => {
                format!("Removed the PATH line from {}.", shown(path))
            }
        });
    }
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&kept_text(&data.kept));
    text
}

fn kept_text(kept: &[Kept]) -> String {
    let mut text = String::new();
    for kept in kept {
        let shown = |path: &str| shown_folder(Path::new(path));
        let line = match kept {
            Kept::Folder { path } => format!("Kept {} and every file in it.", shown(path)),
            Kept::FolderData { path } => format!(
                "Kept {}, which still holds files. Delete it when you no longer need them.",
                shown(path)
            ),
            Kept::LinkedElsewhere { path } => format!(
                "Kept {}, because another device or profile links its folder.",
                shown(path)
            ),
            Kept::DevelopmentBuild { path } => {
                format!("Kept the development build {}.", shown(path))
            }
            Kept::App { path } => format!(
                "Kept {}, which is outside the Applications folders. Move it to the Trash.",
                shown(path)
            ),
            Kept::InFolder { path, folder } => format!(
                "Kept {}, because it holds or is inside the synced folder {}.",
                shown(path),
                shown(folder)
            ),
            Kept::Unrecognized { path } => format!(
                "Kept {}, which holds files Koidrive does not write.",
                shown(path)
            ),
            Kept::LinkTarget { path } => {
                format!("Kept {}, which the koi command linked to.", shown(path))
            }
            Kept::Shared { path } => format!(
                "Kept {}, which another Koidrive profile on this computer uses.",
                shown(path)
            ),
            Kept::PathLine { path } => format!(
                "Kept the PATH line in {}, which another Koidrive profile on this computer uses.",
                shown(path)
            ),
            Kept::OtherProfile { service, argv } => format!(
                "Another Koidrive profile runs the background service {service}. To remove it too, run: {}",
                argv.iter()
                    .map(|word| crate::pairing::shell_word(word))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Kept::LoginItem { path, problem } => format!(
                "Could not remove {} from Login Items ({}). Remove it in System Settings > General > Login Items.",
                shown(path),
                printable(problem)
            ),
        };
        text.push_str(&line);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests;
