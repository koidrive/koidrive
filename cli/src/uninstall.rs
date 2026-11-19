// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use koidrive_driver::{Credential, CredentialError, Flock, Paths, SetAside, StateEntry};
use koidrive_ipc::{Request, Response};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::CliError;
use crate::output::Tone;
use crate::report::{Done, UnconfirmedData};
use crate::service::{Manager, ServiceManager, State};
use crate::setup::shown_folder;
use crate::terminal::Terminal;
use crate::update::{Install, Method};
use crate::{ipc, printable, service};

mod find;
mod remove;
mod text;

use find::{find, is_koi, is_path_line, kept_programs, other_services};
use remove::{
    clean_folder, keep_shared, prune_runtime, quit_apps, released, remove_path, remove_programs,
    remove_service, sign_out, unlink,
};
use text::{finish, kept_text};

const RECORD: &str = "uninstall.json";
const PATH_MARKERS: [&str; 2] = ["; : 'koidrive PATH'", "; true 'koidrive PATH'"];
const BASH_PRELUDE: &str = r#"if [ -r "$HOME/.bash_login" ]; then . "$HOME/.bash_login"; elif [ -r "$HOME/.profile" ]; then . "$HOME/.profile"; fi"#;
const FOLDER_RECORDS: [&str; 6] = [
    "space.json",
    "device.json",
    "device.json.tmp",
    "lineage",
    "lineage.tmp",
    "staging",
];
const RECOVERY_RECORDS: [&str; 2] = ["brought-back.json", "brought-back.json.tmp"];
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
    /// Each version of a file that syncing had set aside in a synced folder's `.koidrive`, now back in its folder under
    /// a recovered name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub brought_back: Vec<String>,
    pub kept: Vec<Kept>,
}

/// Whether this device is still on its server's device list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeviceOutcome {
    NotPaired,
    SignedOut {
        server: String,
        /// Present when the server did not confirm that it revoked this device, so it may still list it.
        #[serde(skip_serializing_if = "Option::is_none")]
        unconfirmed: Option<UnconfirmedData>,
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
    /// A version of a file that syncing set aside in a synced folder's `.koidrive` and that could not be brought back.
    SetAside {
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
    /// A state folder that holds no record of Koidrive creating it, so uninstall left everything in it.
    UnrecordedState {
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
    #[serde(with = "koidrive_driver::raw_path::list")]
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
    unusable: Option<String>,
    signs_out: bool,
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
    unlisted: Option<String>,
    config_in_state: bool,
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
    brought_back: Vec<String>,
    kept: Vec<Kept>,
    problems: Vec<String>,
    next: Vec<Vec<String>>,
    folders: Vec<PathBuf>,
    sign_out_failed: bool,
}

impl Run {
    fn allowed(&mut self, path: &Path) -> bool {
        let Some(folder) = synced_folder(&self.folders, path) else {
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

    fn remove_dir(&mut self, dir: &Path, ours: impl Fn(&Path) -> Option<StateEntry>) {
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
        if !real.is_dir() {
            return self.kept.push(Kept::Unrecognized {
                path: dir.display().to_string(),
            });
        }
        let mut left = Left {
            own: koidrive_state_file::made(real),
            ..Left::default()
        };
        let emptied =
            self.empty(real, Path::new(""), &ours, &mut left)
                .and_then(|empty| match empty {
                    true => fs::remove_dir(real),
                    false => Err(io::ErrorKind::DirectoryNotEmpty.into()),
                });
        let shown_at = |rel: &Path| match rel.as_os_str().is_empty() {
            true => dir.display().to_string(),
            false => dir.join(rel).display().to_string(),
        };
        match emptied {
            Ok(()) if target.is_some() => self.remove(dir, shown),
            Ok(()) => self.removed.push(shown),
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
                self.removed
                    .extend(left.gone.iter().map(|rel| Removed::Files {
                        path: shown_at(rel),
                    }));
                self.kept
                    .extend(left.foreign.iter().map(|rel| Kept::Unrecognized {
                        path: shown_at(rel),
                    }));
            }
            Err(error) => self
                .problems
                .push(format!("cannot remove {}: {error}", shown_folder(dir))),
        }
    }

    fn empty(
        &mut self,
        real: &Path,
        rel: &Path,
        ours: &dyn Fn(&Path) -> Option<StateEntry>,
        left: &mut Left,
    ) -> io::Result<bool> {
        let dir = real.join(rel);
        if left.own {
            koidrive_state_file::recover(&dir)?;
        }
        let mut names = (fs::read_dir(&dir)?.map(|entry| entry.map(|entry| entry.file_name())))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort_by_key(|name| (name == koidrive_state_file::MADE, name == RECORD));
        let mut empty = true;
        let mut foreign = false;
        for name in names {
            let rel = rel.join(name);
            let path = real.join(&rel);
            let folder = fs::symlink_metadata(&path)?.is_dir();
            empty &= match ours(&rel) {
                Some(_) if !self.allowed(&path) => false,
                Some(StateEntry::Folder) if folder => {
                    let mark = left.gone.len();
                    let emptied = self.empty(real, &rel, ours, left)?;
                    if emptied {
                        fs::remove_dir(&path)?;
                        left.gone.truncate(mark);
                        left.gone.push(rel);
                    }
                    emptied
                }
                Some(StateEntry::File) if !folder => {
                    if remove_path(&path)? {
                        left.gone.push(rel);
                    }
                    true
                }
                _ => {
                    foreign = true;
                    false
                }
            };
        }
        if foreign {
            left.foreign.push(rel.to_path_buf());
        }
        Ok(empty)
    }
}

#[derive(Default)]
struct Left {
    own: bool,
    gone: Vec<PathBuf>,
    foreign: Vec<PathBuf>,
}

fn synced_folder(folders: &[PathBuf], path: &Path) -> Option<PathBuf> {
    if fs::symlink_metadata(path).is_err() {
        return None;
    }
    if let Some(folder) = (folders.iter()).find(|folder| holds(folder, path) || holds(path, folder))
    {
        return Some(folder.clone());
    }
    if fs::metadata(path).is_ok_and(|meta| meta.is_dir())
        && fs::symlink_metadata(path.join(".koidrive")).is_ok()
    {
        return Some(path.to_path_buf());
    }
    let parent = path
        .parent()
        .and_then(|parent| fs::canonicalize(parent).ok());
    (path.ancestors().skip(1))
        .chain(parent.iter().flat_map(|parent| parent.ancestors()))
        .find(|dir| fs::symlink_metadata(dir.join(".koidrive/device.json")).is_ok())
        .map(Path::to_path_buf)
}

fn holds(outer: &Path, inner: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if inner.starts_with(outer) {
        return true;
    }
    let Ok(outer) = fs::metadata(outer) else {
        return false;
    };
    let parent = inner
        .parent()
        .and_then(|parent| fs::canonicalize(parent).ok());
    (inner.ancestors())
        .chain(parent.iter().flat_map(|parent| parent.ancestors()))
        .filter_map(|dir| fs::metadata(dir).ok())
        .any(|dir| dir.dev() == outer.dev() && dir.ino() == outer.ino())
}

fn is_state(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(".koidrive")).is_err() && koidrive_state_file::made(dir)
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
            brought_back: Vec::new(),
            kept,
        };
        return Ok(Done::ok(data, text).unchanged());
    }
    let mut found = match confirm(&found, &options, terminal).await? {
        false => found,
        true => {
            let now = find(paths, &home).await;
            if plan(&now, &options) != plan(&found, &options) {
                return Err(CliError::from(
                    "what koi uninstall would remove changed while it waited for your answer, so nothing was changed; run it again to see what it removes now",
                )
                .then(&["koi", "uninstall"]));
            }
            now
        }
    };
    let shared = !found.others.is_empty() || found.unlisted.is_some();
    let mut run = Run {
        kept: found.kept.clone(),
        folders: found.folders.clone(),
        ..Run::default()
    };
    record(paths, &found, &mut run);
    if !run.problems.is_empty() {
        let device = match found.paired {
            true => listed(&found, None),
            false => DeviceOutcome::NotPaired,
        };
        return finish(device, run);
    }
    if !shared {
        quit_apps(&found.programs, &mut run).await;
    }
    let (device, set_aside) = sign_out(paths, &found, &options, terminal, &mut run).await?;
    let set_aside = match set_aside {
        Some(set_aside) => set_aside,
        None => unlink(paths, &found, &mut run).await,
    };
    (run.brought_back)
        .extend((set_aside.brought_back.iter()).map(|path| path.display().to_string()));
    remove_service(&found, &mut run).await;
    let _lock = match released(paths).await {
        Ok(lock) => lock,
        Err(problem) => {
            run.problems.push(problem);
            run.kept
                .extend(set_aside.kept.iter().map(|path| Kept::SetAside {
                    path: path.display().to_string(),
                }));
            return finish(device, run);
        }
    };
    for folder in &found.folders {
        clean_folder(folder, found.owner.as_ref(), &mut run);
    }
    if !run.problems.is_empty() {
        return finish(device, run);
    }
    for file in &found.files {
        if *file == paths.state_dir {
            run.remove_dir(file, |rel| {
                Paths::state_entry(rel).or_else(|| {
                    (found.config_in_state && rel == Path::new("config.toml"))
                        .then_some(StateEntry::File)
                })
            });
        } else if Some(file.as_path()) == paths.config_file.parent() {
            run.remove_dir(file, |rel| {
                (rel == Path::new("config.toml")).then_some(StateEntry::File)
            });
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
    (found.others, found.unlisted) = other_services(paths, &home);
    if !found.others.is_empty() || found.unlisted.is_some() {
        keep_shared(&found, &mut run);
        run.problems.extend(found.unlisted.iter().map(|problem| {
            format!("cannot list the background services of other profiles, so the shared koi stays: {problem}")
        }));
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
    if !run.problems.is_empty() {
        return finish(device, run);
    }
    remove_programs(&found.programs, &found.profiles, &mut run).await;
    finish(device, run)
}

async fn confirm(found: &Found, options: &Options, terminal: &Terminal) -> Result<bool, CliError> {
    if options.yes {
        return Ok(false);
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
        true => Ok(true),
        false => Err("stopped; nothing was changed".into()),
    }
}

fn plan(found: &Found, options: &Options) -> Vec<String> {
    let mut lines = vec!["This removes:".to_owned()];
    if found.signs_out && !options.keep_signed_in {
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
    let shared = !found.others.is_empty() || found.unlisted.is_some();
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
        match kept {
            Kept::Unrecognized { path } => lines.push(format!(
                "It keeps {}, which holds files Koidrive does not write.",
                shown_folder(Path::new(path))
            )),
            Kept::UnrecordedState { path } => lines.push(format!(
                "It keeps {}, which Koidrive has no record of creating.",
                shown_folder(Path::new(path))
            )),
            Kept::InFolder { path, folder } => lines.push(format!(
                "It keeps the PATH line in {}, which is in the synced folder {}.",
                shown_folder(Path::new(path)),
                shown_folder(Path::new(folder))
            )),
            _ => {}
        }
    }
    if found.paired && !found.signs_out && !options.keep_signed_in {
        lines.push(match &found.owner {
            Some(owner) => format!(
                "This device stays signed in to {}.",
                printable(&owner.server)
            ),
            None => "This device stays signed in to its server.".to_owned(),
        });
    }
    lines
}

fn read_record(paths: &Paths) -> Record {
    let mut record: Record = fs::read(paths.state_dir.join(RECORD))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    record.folders.retain(|folder| folder.is_absolute());
    record
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
    let written = koidrive_state_file::write(
        &path,
        &serde_json::to_vec(&record).expect("the record serializes"),
    )
    .and_then(|published| published.durable);
    if let Err(error) = written {
        run.problems
            .push(format!("cannot write {}: {error}", shown_folder(&path)));
    }
}

fn listed(found: &Found, problem: Option<String>) -> DeviceOutcome {
    DeviceOutcome::StillListed {
        server: found.owner.as_ref().map(|owner| owner.server.clone()),
        device_id: found.owner.as_ref().map(|owner| owner.device_id.clone()),
        problem,
    }
}

#[cfg(test)]
mod tests;
