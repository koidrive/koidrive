// SPDX-License-Identifier: Apache-2.0

//! `koi setup` and `koi server ...`: log in with Cloudflare, then deploy a server to the user's account, add
//! this device to the server there, upgrade it or delete it (docs/design/cli-setup.md). The Cloudflare grant lives in
//! memory for one command and is revoked when the command ends, also on failure, Ctrl-C and SIGTERM (section 7).
//!
//! Nothing here asks a question unless [`mode`] says the command is guided: stdin and stderr are terminals, and
//! neither `--json` nor the `CI` variable rules it out, or `--interactive` asks for it.

#[cfg(test)]
mod bundle_files;
mod claim;
#[cfg(test)]
pub(crate) mod fake;
mod state;
#[cfg(feature = "test-setup")]
mod testing;
mod uninstall;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use koidrive_cloudflare::{
    Account, Bundle, CLIENT_ID, Cloudflare, Endpoints, Grant, Jurisdiction, Login, Loopback,
    Script, Secret, UploadPlan,
};
use koidrive_driver::{Credential, DirectoryIdentity, DriverError, LinkApproval, Paths};
use koidrive_ipc::{Code, Next, Request, Response};
use koidrive_net::{Api, ApiError};
use koidrive_proto::v1::{ServerInfo, SpaceInfo};
use tokio::signal::unix::{SignalKind, signal};
use url::Url;

use koidrive_telemetry::schema::{SetupFlow, SetupStep};

use crate::DeviceArgs;
use crate::error::CliError;
use crate::ipc;
use crate::pairing::Invitation;
use crate::printable;
use crate::report::{AccountData, Choice, ChoicesData, Done, FolderData, Outcome, SetupData};
use crate::terminal::{Progress, Terminal, read_line};
use claim::Claim;
use state::{SetupState, StateFile, Step};

const API_BASE: &str = "https://api.cloudflare.com/client/v4/";
/// Marks the Worker scripts `koi setup` uploads (section 6).
const TAG: &str = "koidrive-server";
/// The Durable Object classes that identify an untagged Koidrive Worker, such as a renamed button deploy.
const SERVER_CLASSES: [&str; 2] = ["AccountDO", "SpaceDO"];
/// Cloudflare's error code for an account without an R2 subscription (section 4.1).
const R2_DISABLED: u64 = 10042;
const R2_WAIT: Duration = Duration::from_secs(30 * 60);
const LOGIN_WAIT: Duration = Duration::from_secs(10 * 60);
/// Refresh the access token when it has less than this left, before a step that may take long.
const TOKEN_MARGIN: Duration = Duration::from_secs(5 * 60);
/// How long a new or upgraded Worker may take to answer, and to report its new version.
const SERVE_WAIT: Duration = Duration::from_secs(90);
const DEFAULT_FOLDER: &str = "Koidrive";

include!(concat!(env!("OUT_DIR"), "/server_bundle.rs"));

/// Whether a Cloudflare login session is live. Its own signal handling revokes the grant before the command ends,
/// so main's Ctrl-C handling stands aside while it is.
static SESSION: AtomicBool = AtomicBool::new(false);

pub fn in_session() -> bool {
    SESSION.load(Ordering::Relaxed)
}

#[derive(clap::Args)]
pub struct LoginArgs {
    /// Ask questions on the terminal, as the guided setup does. Needs a terminal on stdin and stderr, and cannot be
    /// used with --json.
    #[arg(long)]
    pub(crate) interactive: bool,
    /// Print the login page instead of opening a browser, and read the address the browser ends on from the terminal.
    /// Needs --interactive.
    #[arg(long)]
    no_browser: bool,
    /// The local port for the login callback, for example to forward it over SSH. Defaults to a free port.
    #[arg(long)]
    port: Option<u16>,
}

#[derive(clap::Subcommand)]
pub enum ServerAction {
    /// Log in with Cloudflare, deploy a new server and claim it with this device.
    Deploy {
        #[command(flatten)]
        device: DeviceArgs,
        #[command(flatten)]
        login: LoginArgs,
    },
    /// Log in with Cloudflare and add this device to the server in the account, without a pairing code.
    AddDevice {
        /// The server's Worker name, when the account holds several.
        #[arg(long)]
        worker: Option<String>,
        #[command(flatten)]
        device: DeviceArgs,
        #[command(flatten)]
        login: LoginArgs,
    },
    /// Claim a server deployed without koi setup, with the one-time claim code that its /setup page writes to the
    /// Worker's log. This device becomes the owner's first device.
    #[command(group(clap::ArgGroup::new("code_source").required(true).args(["code", "code_stdin"])))]
    Claim {
        /// The server's address, such as https://koidrive.alice.workers.dev.
        #[arg(value_name = "SERVER_URL")]
        server: String,
        /// The claim code from the Worker's log.
        code: Option<String>,
        /// Read the claim code from one line of stdin rather than an argument.
        #[arg(long)]
        code_stdin: bool,
        /// Your name, which the server keeps for its owner, at most 255 bytes.
        #[arg(long, value_parser = crate::short_name)]
        user: String,
        #[command(flatten)]
        device: DeviceArgs,
    },
    /// Log in with Cloudflare and upload this CLI's server version. Without a terminal it needs --worker and --yes.
    Upgrade {
        /// The server's Worker name, when the account holds several.
        #[arg(long)]
        worker: Option<String>,
        /// Go on without asking, for example past the Workers Builds warning.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        login: LoginArgs,
    },
    /// Log in with Cloudflare, delete the server's Worker, then empty and delete its bucket: every file, trashed file
    /// and version on the server is lost. Without a terminal it needs --worker or --bucket, and --yes.
    Delete {
        /// The server's Worker name, when the account holds several.
        #[arg(long)]
        worker: Option<String>,
        /// Empty and delete this bucket when its Worker is already gone.
        #[arg(long)]
        bucket: Option<String>,
        /// The jurisdiction of --bucket: default, eu, us, fedramp or fedramp-high.
        #[arg(long, requires = "bucket", value_parser = jurisdiction, default_value = "default")]
        jurisdiction: Jurisdiction,
        /// Delete without asking.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        login: LoginArgs,
    },
}

#[derive(clap::Args)]
pub struct ScriptArgs {
    /// Read a pairing code, written as code@host, from stdin and connect this computer with it. Needs --folder.
    #[arg(long, requires = "folder")]
    pairing_code_stdin: bool,
    /// Sync this folder without asking any question, and create it when it is missing. Needs --space.
    #[arg(
        long,
        value_name = "PATH",
        requires = "space",
        conflicts_with_all = ["interactive", "no_browser"]
    )]
    folder: Option<PathBuf>,
    /// The shared folder to sync with --folder: the ID or name of a space, as koi space list shows it.
    #[arg(long, value_name = "SPACE", requires = "folder")]
    space: Option<String>,
    /// Link a --folder that already holds files. Its files upload, and the shared folder's files download into it.
    #[arg(long, requires = "folder")]
    accept_existing_files: bool,
}

impl ScriptArgs {
    pub(crate) fn given(&self) -> bool {
        self.folder.is_some()
    }
}

/// How a setup or server command runs: guided, asking questions on the terminal, or direct, never asking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Guided,
    Direct,
}

/// The mode of a setup or server command. It is guided only with a terminal on both stdin and stderr, and then when
/// `--interactive` asks for it, or when neither `--json` nor the `CI` variable rules it out. `--interactive` without
/// those terminals is a usage error, since nobody could answer.
pub fn mode(
    stdin_tty: bool,
    stderr_tty: bool,
    json: bool,
    ci: bool,
    interactive: bool,
) -> Result<Mode, CliError> {
    let terminal = stdin_tty && stderr_tty;
    if interactive && json {
        return Err(CliError::usage("--interactive cannot be used with --json"));
    }
    if interactive && !terminal {
        return Err(CliError::usage(
            "--interactive asks questions, so it needs a terminal on stdin and stderr",
        ));
    }
    match interactive || (terminal && !json && !ci) {
        true => Ok(Mode::Guided),
        false => Ok(Mode::Direct),
    }
}

fn this_mode(json: bool, interactive: bool) -> Result<Mode, CliError> {
    mode(
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
        json,
        std::env::var_os("CI").is_some(),
        interactive,
    )
}

/// The ways to set up this machine, which `koi setup` lists when it does not guide.
fn choices() -> Vec<Choice> {
    let choice = |argv: &[&str], description| Choice {
        argv: Next::of(argv).argv,
        description,
    };
    vec![
        choice(
            &["koi", "join", "<CODE>"],
            "Join a server with a pairing code from a device that is already paired",
        ),
        choice(
            &["koi", "server", "deploy", "--interactive"],
            "Log in with Cloudflare and deploy a server to your account",
        ),
        choice(
            &["koi", "server", "add-device", "--interactive"],
            "Log in with Cloudflare and add this device to the server there, without a pairing code",
        ),
        choice(
            &[
                "koi",
                "server",
                "claim",
                "<SERVER_URL>",
                "<CODE>",
                "--user",
                "<NAME>",
            ],
            "Claim a server deployed without koi, with the code in its Worker's log",
        ),
        choice(
            &["koi", "setup", "--interactive"],
            "Choose in the guided setup, on a terminal",
        ),
    ]
}

/// `koi setup`: on a terminal, asks whether to join with a code or log in with Cloudflare. Otherwise it lists the
/// commands that set up this machine, and changes nothing.
pub async fn setup(
    paths: &Paths,
    device_name: &str,
    login: &LoginArgs,
    script: &ScriptArgs,
    json: bool,
    terminal: Arc<Terminal>,
) -> Result<Done, CliError> {
    if let (Some(folder), Some(space)) = (&script.folder, &script.space) {
        return scripted(paths, device_name, script, folder, space).await;
    }
    let mode = this_mode(json, login.interactive)?;
    if login.no_browser && !login.interactive {
        return Err(CliError::new(
            Code::AuthRequired,
            "--no-browser reads the login address from the terminal, so it needs --interactive",
        )
        .then(&["koi", "setup", "--interactive", "--no-browser"]));
    }
    if mode == Mode::Direct {
        not_set_up(paths)?;
        let choices = choices();
        let mut text = "Set up Koidrive on this machine with one of these commands:\n".to_owned();
        for choice in &choices {
            text.push_str(&format!(
                "  {}\n      {}\n",
                choice.argv.join(" "),
                choice.description
            ));
        }
        let next = (choices.iter())
            .map(|choice| Next {
                argv: choice.argv.clone(),
            })
            .collect();
        let mut done = Done::ok(ChoicesData { choices }, text);
        done.next = next;
        return Ok(done);
    }
    terminal.intro();
    if connected(paths) {
        terminal.complete("This device is already connected");
        let folders = match ipc::run(paths, Request::Status).await? {
            Response::Status { status } => status.folders,
            other => return Err(crate::unexpected(other)),
        };
        if !folders.is_empty() {
            for folder in &folders {
                terminal.complete(&format!(
                    "{} is linked to {}",
                    shown_folder(Path::new(&folder.path)),
                    folder.space_name
                ));
            }
            return Ok(Done {
                note: "Setup is already complete. Your folders have not changed.\nCheck sync: koi status\n".into(),
                ..Done::printed()
            });
        }
    } else {
        connect(paths, device_name, login, terminal.clone()).await?;
    }
    onboard(paths, &terminal).await
}

async fn connect(
    paths: &Paths,
    device_name: &str,
    login: &LoginArgs,
    terminal: Arc<Terminal>,
) -> Result<(), CliError> {
    let route = terminal
        .select(
            "How do you want to connect?",
            vec![
                "Log in with Cloudflare".into(),
                "Enter a pairing code".into(),
            ],
        )
        .await?;
    match route {
        0 => {
            crate::telemetry::set_flow(SetupFlow::Cloudflare);
            bundled()?;
            with_session(login, terminal.clone(), async |session| {
                session.setup(paths, device_name).await
            })
            .await?;
            terminal.complete("This device is connected");
            Ok(())
        }
        1 => {
            crate::telemetry::set_flow(SetupFlow::Pairing);
            terminal.separator();
            terminal.message("On your other device, run koi device invite.");
            let code = terminal.input("Pairing code", true).await?;
            let progress = terminal.progress("Connecting this device...");
            let joining = async {
                let invitation = invitation(&code)?;
                let request = Request::Pair {
                    server: invitation.server,
                    code: invitation.code,
                    name: device_name.to_owned(),
                };
                report_paired(paths, ipc::run(paths, request).await?, &terminal)
            };
            crate::telemetry::step(SetupStep::Pair, joining).await?;
            progress.finish("This device is connected");
            Ok(())
        }
        _ => Err(CliError::from(
            "the terminal returned an invalid setup choice",
        )),
    }
}

async fn onboard(paths: &Paths, terminal: &Terminal) -> Result<Done, CliError> {
    let credential = Credential::load(&paths.credential())
        .map_err(|error| error.to_string())?
        .ok_or(DriverError::NotPaired)?;
    let auth = crate::reading(paths, &credential)?;
    let mut progress = Some(terminal.progress("Preparing ~/Koidrive..."));
    let spaces = auth.api().list_spaces(&auth).await?.spaces;
    let mut target = if spaces.is_empty() {
        Target::Default
    } else {
        drop(progress.take());
        shared_folder(terminal, &spaces).await?
    };
    let named = loop {
        if progress.is_none() {
            progress = Some(terminal.continue_progress("Preparing ~/Koidrive..."));
        }
        match target {
            Target::Existing(space) => break crate::Named::Existing(space),
            Target::Default => match crate::named_space(&auth, &spaces, DEFAULT_FOLDER).await? {
                crate::Named::Created(space) => break crate::Named::Created(space),
                crate::Named::Existing(_) => {
                    drop(progress.take());
                    terminal.message(&format!(
                        "A shared folder named {DEFAULT_FOLDER} is available. Choose which folder to sync."
                    ));
                    let now = auth.api().list_spaces(&auth).await?;
                    target = shared_folder(terminal, &now.spaces).await?;
                }
            },
            Target::New(name) => match crate::create_space(&auth, &name).await {
                Ok(space) => break crate::Named::Created(space),
                Err(error) if error.status() != Some(409) => return Err(error.into()),
                Err(error) => {
                    let now = auth.api().list_spaces(&auth).await?;
                    if !now.spaces.iter().any(|space| space.name == name) {
                        return Err(error.into());
                    }
                    drop(progress.take());
                    terminal.message(&format!(
                        "A shared folder named {} already exists. Choose another name.",
                        printable(&name)
                    ));
                    target = new_shared_folder(terminal, &now.spaces).await?;
                }
            },
        }
    };
    let SelectedFolder {
        folder,
        local,
        directory,
    } = local_folder(terminal, &named, &mut progress).await?;
    let preparing = format!("Preparing {}...", shown_folder(&folder));
    let linking = match progress.take() {
        Some(progress) => progress,
        None if folder == home()?.join(DEFAULT_FOLDER) => terminal.continue_progress(&preparing),
        None => terminal.progress(&preparing),
    };
    if local == Local::Missing {
        create(&folder)?;
    }
    let directory = directory
        .map(Ok)
        .unwrap_or_else(|| directory_identity(&folder))?;
    let approval = LinkApproval {
        directory,
        accept_existing_files: local == Local::Files,
    };
    let (status, _) = crate::link(
        paths,
        folder.clone(),
        named.space().space_id.clone(),
        Some(approval),
    )
    .await?;
    linking.finish(&format!(
        "{} is linked to {}",
        shown_folder(Path::new(&status.path)),
        status.space_name
    ));
    let data = SetupData {
        account: AccountData::of(&credential.account()),
        folder: FolderData::of(&status),
    };
    Ok(Done {
        note: ending(&folder),
        ..Done::ok(data, "")
    })
}

enum Target {
    Existing(SpaceInfo),
    Default,
    New(String),
}

async fn shared_folder(terminal: &Terminal, spaces: &[SpaceInfo]) -> Result<Target, CliError> {
    let mut items: Vec<String> = (spaces.iter())
        .map(|space| {
            let twins = (spaces.iter())
                .filter(|other| other.name == space.name)
                .count();
            match twins {
                1 => space.name.clone(),
                _ => format!("{} ({})", space.name, short_id(&space.space_id)),
            }
        })
        .collect();
    items.push("Create a shared folder".to_owned());
    let index = terminal
        .select("Which shared folder should sync here?", items)
        .await?;
    if let Some(space) = spaces.get(index) {
        return Ok(Target::Existing(space.clone()));
    }
    new_shared_folder(terminal, spaces).await
}

async fn new_shared_folder(terminal: &Terminal, spaces: &[SpaceInfo]) -> Result<Target, CliError> {
    loop {
        let answer = terminal.input("Shared folder name", false).await?;
        let name = answer.trim();
        match crate::short_name(name) {
            Ok(_) if name.is_empty() => terminal.message("Enter a name for the shared folder."),
            Ok(_) if spaces.iter().any(|space| space.name == name) => terminal.message(&format!(
                "A shared folder named {} already exists. Choose another name.",
                printable(name)
            )),
            Ok(_) => {
                return Ok(Target::New(name.to_owned()));
            }
            Err(problem) => terminal.message(&problem),
        }
    }
}

fn short_id(id: &str) -> &str {
    match id.char_indices().rev().nth(7) {
        Some((at, _)) => &id[at..],
        None => id,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Local {
    Missing,
    Empty,
    Files,
}

struct SelectedFolder {
    folder: PathBuf,
    local: Local,
    directory: Option<DirectoryIdentity>,
}

enum Entries {
    Syncable,
    All,
}

fn directory_identity(folder: &Path) -> Result<DirectoryIdentity, CliError> {
    std::fs::metadata(folder)
        .map(|meta| DirectoryIdentity::of(&meta))
        .map_err(|error| {
            CliError::from(format!(
                "{}: {error}",
                printable(&folder.display().to_string())
            ))
        })
}

async fn local_folder(
    terminal: &Terminal,
    named: &crate::Named,
    progress: &mut Option<Progress<'_>>,
) -> Result<SelectedFolder, CliError> {
    let mut folder = home()?.join(DEFAULT_FOLDER);
    loop {
        let local = match inspect(&folder, Entries::Syncable) {
            Ok(local) => local,
            Err(problem) => {
                drop(progress.take());
                terminal.message(&format!("{problem}. Choose another folder."));
                folder = read_folder(terminal).await?;
                continue;
            }
        };
        let directory = (local != Local::Missing)
            .then(|| directory_identity(&folder))
            .transpose()?;
        if local != Local::Files {
            return Ok(SelectedFolder {
                folder,
                local,
                directory,
            });
        }
        drop(progress.take());
        let path = shown_folder(&folder);
        let name = &named.space().name;
        terminal.separator();
        match named {
            crate::Named::Existing(_) => {
                terminal.message(&format!(
                    "{path} contains files. They will upload to {name}."
                ));
                terminal.message(&format!("Files in {name} will download into {path}."));
            }
            crate::Named::Created(_) => terminal.message(&format!(
                "{path} contains files. They will upload to a new shared folder named {name}."
            )),
        }
        if terminal
            .select(
                "What should Koidrive do?",
                vec!["Use another folder".into(), "Sync these files".into()],
            )
            .await?
            == 1
        {
            return Ok(SelectedFolder {
                folder,
                local,
                directory,
            });
        }
        folder = read_folder(terminal).await?;
    }
}

async fn read_folder(terminal: &Terminal) -> Result<PathBuf, CliError> {
    loop {
        let answer = terminal.input("Folder to sync", false).await?;
        if answer.trim().is_empty() {
            terminal.message("Enter a folder path.");
            continue;
        }
        return crate::absolute(&expanded(&typed(&answer))?);
    }
}

fn inspect(folder: &Path, entries: Entries) -> Result<Local, String> {
    let shown = printable(&folder.display().to_string());
    let unreadable = |error: std::io::Error| format!("{shown} cannot be read: {error}");
    if std::fs::symlink_metadata(folder).is_ok_and(|meta| meta.file_type().is_symlink()) {
        std::fs::metadata(folder).map_err(unreadable)?;
    }
    match std::fs::metadata(folder) {
        Ok(meta) if meta.is_dir() => {
            if rustix::fs::access(
                folder,
                rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK,
            )
            .is_err()
            {
                return Err(format!("{shown} is not writable"));
            }
            let files = match entries {
                Entries::Syncable => {
                    koidrive_driver::ignore::has_sync_entries(folder).map_err(unreadable)?
                }
                Entries::All => std::fs::read_dir(folder)
                    .map_err(unreadable)?
                    .next()
                    .transpose()
                    .map_err(unreadable)?
                    .is_some(),
            };
            Ok(if files { Local::Files } else { Local::Empty })
        }
        Ok(_) => Err(format!("{shown} is a file, not a folder")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = folder
                .ancestors()
                .skip(1)
                .find(|dir| std::fs::symlink_metadata(dir).is_ok());
            if let Some(parent) = parent {
                std::fs::metadata(parent).map_err(unreadable)?;
            }
            match parent {
                Some(dir) if !dir.is_dir() => Err(format!(
                    "{shown} cannot be created, because {} is a file",
                    printable(&dir.display().to_string())
                )),
                Some(dir)
                    if rustix::fs::access(
                        dir,
                        rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK,
                    )
                    .is_err() =>
                {
                    Err(format!(
                        "{shown} cannot be created, because {} is not writable",
                        printable(&dir.display().to_string())
                    ))
                }
                _ => Ok(Local::Missing),
            }
        }
        Err(error) => Err(unreadable(error)),
    }
}

fn create(folder: &Path) -> Result<(), CliError> {
    std::fs::create_dir_all(folder).map_err(|error| {
        CliError::from(format!(
            "{} cannot be created: {error}",
            printable(&folder.display().to_string())
        ))
    })
}

fn home() -> Result<PathBuf, CliError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or_else(|| CliError::from("HOME is not set to an absolute path"))
}

fn typed(answer: &str) -> PathBuf {
    let text = answer.trim();
    for quote in ['\'', '"'] {
        if let Some(inner) = (text.strip_prefix(quote)).and_then(|text| text.strip_suffix(quote)) {
            return PathBuf::from(inner);
        }
    }
    let mut path = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        path.push(match c {
            '\\' => chars.next().unwrap_or(c),
            c => c,
        });
    }
    PathBuf::from(path)
}

fn expanded(path: &Path) -> Result<PathBuf, CliError> {
    match path.strip_prefix("~") {
        Ok(rest) => Ok(home()?.join(rest)),
        Err(_) => Ok(path.to_owned()),
    }
}

fn ending(folder: &Path) -> String {
    format!(
        "Setup complete. Add files to {}.\nCheck sync: koi status\n",
        printable(&shown_folder(folder))
    )
}

pub(crate) fn shown_folder(folder: &Path) -> String {
    if let Ok(home) = home()
        && let Ok(rest) = folder.strip_prefix(home)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.display())
        };
    }
    folder.display().to_string()
}

async fn scripted(
    paths: &Paths,
    device_name: &str,
    script: &ScriptArgs,
    folder: &Path,
    space: &str,
) -> Result<Done, CliError> {
    if space.trim().is_empty() {
        return Err(CliError::usage("--space needs the ID or name of a space"));
    }
    let folder = crate::absolute(&expanded(folder)?)?;
    let local = inspect(&folder, Entries::All).map_err(CliError::usage)?;
    let directory = (local != Local::Missing)
        .then(|| directory_identity(&folder))
        .transpose()?;
    let credential = Credential::load(&paths.credential()).map_err(|error| error.to_string())?;
    if local == Local::Files && !script.accept_existing_files {
        let linked = if credential.is_some() {
            let root = std::fs::canonicalize(&folder).map_err(|error| {
                CliError::usage(format!(
                    "{}: {error}",
                    printable(&folder.display().to_string())
                ))
            })?;
            match ipc::run(paths, Request::Status).await? {
                Response::Status { status } => status
                    .folders
                    .iter()
                    .any(|folder| Path::new(&folder.path) == root),
                other => return Err(crate::unexpected(other)),
            }
        } else {
            false
        };
        if !linked {
            return Err(CliError::usage(format!(
                "{} already holds files, which would upload to the shared folder while its files download here; add --accept-existing-files to link it",
                printable(&folder.display().to_string())
            )));
        }
    }
    let code = match script.pairing_code_stdin {
        true => Some(read_invitation().await?),
        false => None,
    };
    let (account, paired) = match (credential, code) {
        (Some(credential), Some(code)) if !same_server(&credential.server, &code.server) => {
            return Err(CliError::usage(format!(
                "this computer is already connected to {}, and the pairing code is for {}",
                printable(&credential.server),
                printable(&code.server)
            )));
        }
        (Some(credential), _) => (credential.account(), false),
        (None, Some(code)) => {
            crate::telemetry::set_flow(SetupFlow::Pairing);
            let joining = pair(paths, code, device_name);
            (
                crate::telemetry::step(SetupStep::Pair, joining).await?,
                true,
            )
        }
        (None, None) => {
            return Err(CliError::new(
                Code::AuthRequired,
                "this computer is not connected to Koidrive yet; give a pairing code on stdin with --pairing-code-stdin",
            ));
        }
    };
    if local == Local::Missing {
        create(&folder)?;
    }
    let directory = directory
        .map(Ok)
        .unwrap_or_else(|| directory_identity(&folder))?;
    let approval = LinkApproval {
        directory,
        accept_existing_files: script.accept_existing_files,
    };
    let (status, outcome) =
        crate::link(paths, folder.clone(), space.to_owned(), Some(approval)).await?;
    let data = SetupData {
        account: AccountData::of(&account),
        folder: FolderData::of(&status),
    };
    let done = Done {
        note: format!(
            "Add files to {} to sync them.\nCheck sync: koi status\n",
            printable(&folder.display().to_string())
        ),
        ..Done::ok(data, crate::linked(&status, outcome))
    }
    .human(
        crate::linked_summary(&status, outcome),
        format!(
            "Add files to {} to sync them.\nCheck sync: koi status\n",
            crate::output::path(&folder)
        ),
    );
    Ok(
        match paired || local == Local::Missing || outcome != Outcome::Unchanged {
            true => done,
            false => done.unchanged(),
        },
    )
}

async fn read_invitation() -> Result<Invitation, CliError> {
    let line = tokio::task::spawn_blocking(|| read_line(std::io::stdin().lock()))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|_| CliError::usage("--pairing-code-stdin found no pairing code on stdin"))?;
    invitation(&line)
}

fn invitation(code: &str) -> Result<Invitation, CliError> {
    crate::pairing::parse(code, None)
        .map_err(|_| CliError::usage("Copy the full pairing code, including its @server address"))
}

fn same_server(one: &str, other: &str) -> bool {
    match (Url::parse(one), Url::parse(other)) {
        (Ok(one), Ok(other)) => one.origin() == other.origin(),
        _ => one == other,
    }
}

async fn pair(
    paths: &Paths,
    code: Invitation,
    name: &str,
) -> Result<koidrive_driver::Account, CliError> {
    let request = Request::Pair {
        server: code.server,
        code: code.code,
        name: name.to_owned(),
    };
    match ipc::run(paths, request).await? {
        Response::Paired { account } => Ok(account),
        other => Err(crate::unexpected(other)),
    }
}

impl ServerAction {
    pub(crate) fn login(&self) -> Option<&LoginArgs> {
        match self {
            ServerAction::Deploy { login, .. }
            | ServerAction::AddDevice { login, .. }
            | ServerAction::Upgrade { login, .. }
            | ServerAction::Delete { login, .. } => Some(login),
            ServerAction::Claim { .. } => None,
        }
    }

    /// The same command with `--interactive`, for a person to run on a terminal. It keeps every argument that picks
    /// what the command acts on, so that it reaches the same Worker, bucket and jurisdiction, and drops `--yes`, so
    /// that the person is asked.
    fn interactive_argv(&self) -> Vec<String> {
        let (name, worker, bucket, jurisdiction, device) = match self {
            ServerAction::Deploy { device, .. } => ("deploy", None, None, None, Some(device)),
            ServerAction::AddDevice { worker, device, .. } => {
                ("add-device", worker.as_deref(), None, None, Some(device))
            }
            ServerAction::Upgrade { worker, .. } => {
                ("upgrade", worker.as_deref(), None, None, None)
            }
            ServerAction::Delete {
                worker,
                bucket,
                jurisdiction,
                ..
            } => (
                "delete",
                worker.as_deref(),
                bucket.as_deref(),
                Some(jurisdiction.as_str()).filter(|name| *name != "default"),
                None,
            ),
            ServerAction::Claim { .. } => ("claim", None, None, None, None),
        };
        let mut argv = Next::of(&["koi", "server", name]).argv;
        let port = self
            .login()
            .and_then(|login| login.port)
            .map(|port| port.to_string());
        let named = [
            ("--worker", worker),
            ("--bucket", bucket),
            ("--jurisdiction", jurisdiction),
            ("--name", device.and_then(|device| device.name.as_deref())),
        ];
        for (flag, value) in named {
            if let Some(value) = value {
                argv.extend([flag.to_owned(), value.to_owned()]);
            }
        }
        argv.push("--interactive".to_owned());
        if self.login().is_some_and(|login| login.no_browser) {
            argv.push("--no-browser".to_owned());
        }
        if let Some(port) = port {
            argv.extend(["--port".to_owned(), port]);
        }
        argv
    }
}

/// `koi server <action>`. Each logs in with Cloudflare in a browser, so it runs only with `--interactive` and a
/// terminal on stdin and stderr. Otherwise, with or without `--json`, it stops before the login and names the
/// `--interactive` command for a person to run.
pub async fn server(
    paths: &Paths,
    action: ServerAction,
    json: bool,
    terminal: Arc<Terminal>,
) -> Result<Done, CliError> {
    let login = action.login().expect("main sends a claim to the driver");
    if login.interactive && json {
        return Err(CliError::usage("--interactive cannot be used with --json"));
    }
    let attended = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if !(login.interactive && attended) {
        let argv = action.interactive_argv();
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        let message = format!(
            "this command logs in with Cloudflare in a browser; a person runs {} on a terminal",
            argv.join(" ")
        );
        return Err(CliError::new(Code::AuthRequired, message).then(&words));
    }
    match action {
        ServerAction::Deploy { device, login } => {
            not_set_up(paths)?;
            bundled()?;
            let name = device.name();
            with_session(&login, terminal.clone(), async |session| {
                session.server_deploy(paths, &name).await
            })
            .await?;
        }
        ServerAction::AddDevice {
            worker,
            device,
            login,
        } => {
            not_set_up(paths)?;
            bundled()?;
            let name = device.name();
            with_session(&login, terminal.clone(), async |session| {
                session
                    .server_add_device(paths, worker.as_deref(), &name)
                    .await
            })
            .await?;
        }
        ServerAction::Claim { .. } => unreachable!("main sends a claim to the driver"),
        ServerAction::Upgrade { worker, yes, login } => {
            bundled()?;
            with_session(&login, terminal.clone(), async |session| {
                let found = session.pick(worker.as_deref()).await?;
                crate::telemetry::step(SetupStep::Upgrade, session.upgrade(&found, yes)).await
            })
            .await?;
        }
        ServerAction::Delete {
            worker,
            bucket,
            jurisdiction,
            yes,
            login,
        } => {
            let resuming = StateFile::new(paths)
                .load()?
                .is_some_and(|state| state.step == Step::Uninstalling);
            if bucket.is_none() && !resuming {
                bundled()?;
            }
            with_session(&login, terminal.clone(), async |session| {
                let uninstall = session.uninstall(
                    paths,
                    worker.as_deref(),
                    bucket.as_deref(),
                    jurisdiction,
                    yes,
                );
                crate::telemetry::step(SetupStep::Remove, uninstall).await
            })
            .await?;
        }
    }
    Ok(Done::printed())
}

fn jurisdiction(text: &str) -> Result<Jurisdiction, String> {
    [
        Jurisdiction::Default,
        Jurisdiction::Eu,
        Jurisdiction::Us,
        Jurisdiction::Fedramp,
        Jurisdiction::FedrampHigh,
    ]
    .into_iter()
    .find(|jurisdiction| jurisdiction.as_str() == text)
    .ok_or_else(|| "not one of default, eu, us, fedramp and fedramp-high".to_owned())
}

/// Refuses a machine that is already set up, unless a run that stopped halfway left its state file: that run checks
/// the saved credential before it claims (section 4.6).
fn not_set_up(paths: &Paths) -> Result<(), CliError> {
    if connected(paths) {
        return Err(format!(
            "this device is already set up; its credential is {}",
            paths.credential().display()
        )
        .into());
    }
    Ok(())
}

fn connected(paths: &Paths) -> bool {
    paths.credential().exists() && !StateFile::new(paths).path().exists()
}

/// Why a new deploy or claim must not replace `state`, the state file of a run in `account` or another one that has
/// not finished: the file is that run's only record of what it created.
fn unfinished(state: &SetupState, account: &str, file: &Path) -> CliError {
    let run = if state.account != account {
        format!(
            "a setup of {} in another Cloudflare account ({}) has not finished; run koi setup and log in to that account to finish it",
            state.script, state.account
        )
    } else if state.step == Step::Uninstalling {
        format!(
            "the delete of {} has not finished; run koi server delete to finish it",
            state.script
        )
    } else {
        format!(
            "the deploy of {} has not finished; run koi setup to finish it",
            state.script
        )
    };
    format!("{run}, or delete {} to forget it", file.display()).into()
}

fn report_paired(paths: &Paths, response: Response, terminal: &Terminal) -> Result<(), CliError> {
    match response {
        Response::Paired { account } => {
            if terminal.setup_guided() {
                return Ok(());
            }
            println!(
                "This device is set up as {} ({}) with {}.\nThe credential is in {}.",
                printable(&account.device_name),
                printable(&account.device_id),
                printable(&account.server),
                paths.credential().display()
            );
            Ok(())
        }
        other => Err(format!("the daemon answered {other:?}").into()),
    }
}

/// Logs in, runs `work`, and revokes the grant however `work` ends: success, failure, Ctrl-C or SIGTERM. After a
/// signal it exits at once with 130 for Ctrl-C or 143 for SIGTERM, as a shell does for a command the signal ended.
async fn with_session(
    args: &LoginArgs,
    terminal: Arc<Terminal>,
    work: impl AsyncFnOnce(&mut Session) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let mut terminate = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|error| error.to_string())?;
    let mut status = None;
    let stopped = async || {
        let (how, code) = tokio::select! {
            _ = terminate.recv() => ("stopped by SIGTERM", 143),
            _ = interrupt.recv() => ("stopped", 130),
        };
        status = Some(code);
        how
    };
    SESSION.store(true, Ordering::Relaxed);
    let login = crate::telemetry::step(SetupStep::Login, Session::login(args, terminal.clone()));
    let outcome = guarded(login, stopped, work).await;
    terminal.cancel().await;
    SESSION.store(false, Ordering::Relaxed);
    if let (Err(error), Some(code)) = (&outcome, status) {
        crate::telemetry::flush();
        eprintln!("koi: {}", printable(&error.message));
        std::process::exit(code);
    }
    outcome
}

/// Runs `work` on the session `login` returns, then revokes its grant. The grant exists once `login` returns, so
/// everything after that, the account lookup included, ends with the revocation, also when `stopped` fires.
async fn guarded(
    login: impl Future<Output = Result<Session, CliError>>,
    mut stopped: impl AsyncFnMut() -> &'static str,
    work: impl AsyncFnOnce(&mut Session) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let mut session = tokio::select! {
        session = login => session?,
        how = stopped() => return Err(format!("{how} before the login finished").into()),
    };
    let outcome = tokio::select! {
        outcome = async {
            session.pick_account().await?;
            work(&mut session).await
        } => outcome,
        how = stopped() => Err(format!("{how}; run the same command again to continue").into()),
    };
    let terminal = session.terminal.clone();
    let _progress = terminal
        .setup_guided()
        .then(|| terminal.continue_progress("Connecting this device..."));
    session.end().await;
    outcome
}

pub(crate) async fn read_code(code: Option<String>, code_stdin: bool) -> Result<String, CliError> {
    if !code_stdin {
        return Ok(code.expect("clap requires a code or --code-stdin"));
    }
    let code = read_answer(|| read_line(std::io::stdin().lock())).await?;
    if code.is_empty() {
        return Err(CliError::usage("stdin contains no code"));
    }
    Ok(code)
}

async fn read_answer(
    read: impl FnOnce() -> std::io::Result<String> + Send + 'static,
) -> Result<String, CliError> {
    let (sender, answer) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read());
    });
    let line = answer
        .await
        .map_err(|_| "the answer could not be read".to_owned())?
        .map_err(|error| error.to_string())?;
    Ok(line.trim().to_owned())
}

async fn confirm(
    terminal: &Terminal,
    question: &str,
    yes: bool,
    flag: &str,
) -> Result<(), CliError> {
    if yes {
        return Ok(());
    }
    if !terminal.guided() {
        return Err(
            format!("{question} Run again with {flag} to go on without a terminal.").into(),
        );
    }
    match terminal.confirm(question, false).await? {
        true => Ok(()),
        false => Err("stopped; nothing more was changed".into()),
    }
}

/// Opens `url` in the default browser, without a shell. A failure only means the user opens it by hand.
async fn open_browser(url: &Url) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let opened = tokio::process::Command::new(opener)
        .arg(url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    tokio::time::timeout(Duration::from_secs(5), opened)
        .await
        .is_ok_and(|result| result.is_ok_and(|status| status.success()))
}

/// A Koidrive server found in the account.
struct Found {
    script: Script,
    bucket: Option<Bound>,
    url: Url,
    info: Option<ServerInfo>,
}

/// The R2 bucket a server's Worker binds.
#[derive(Clone)]
struct Bound {
    name: String,
    jurisdiction: Jurisdiction,
}

/// The Cloudflare account a command acts on, with an access token that is fresh for the next calls.
trait Cloud {
    fn account(&self) -> &str;
    async fn fresh(&mut self) -> Result<Cloudflare, CliError>;
}

/// One command's Cloudflare login: the grant and the account the user picked on the consent page.
struct Session {
    terminal: Arc<Terminal>,
    http: reqwest::Client,
    grant: Grant,
    /// The Cloudflare API's base URL. Tests point it at a local server.
    api_base: Url,
    account: Account,
    subdomain: Option<String>,
}

impl Cloud for Session {
    fn account(&self) -> &str {
        &self.account.id
    }

    /// Refreshes the access token when it is about to expire. A refresh that fails ends the command, because a retry
    /// with the old refresh token would revoke the grant (section 2).
    async fn fresh(&mut self) -> Result<Cloudflare, CliError> {
        if self.grant.expires_within(TOKEN_MARGIN) {
            self.grant.refresh(&self.http).await.map_err(|error| {
                CliError::caused(
                    format!("{error}; run the command again to log in again"),
                    error,
                )
            })?;
        }
        Ok(self.api())
    }
}

impl Session {
    /// Logs in with Cloudflare and returns once the code is exchanged for a grant, before any other call.
    async fn login(args: &LoginArgs, terminal: Arc<Terminal>) -> Result<Session, CliError> {
        #[cfg(feature = "test-setup")]
        let testing = testing::config()?;
        let http = koidrive_cloudflare::http_client().map_err(|error| error.to_string())?;
        #[cfg(feature = "test-setup")]
        let http = if testing.is_some() {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|error| error.to_string())?
        } else {
            http
        };
        let loopback = if args.no_browser {
            None
        } else {
            Some(Loopback::bind(args.port).await?)
        };
        let port = match &loopback {
            Some(loopback) => loopback.port(),
            None => args.port.unwrap_or(8976),
        };
        let endpoints = Endpoints::cloudflare();
        #[cfg(feature = "test-setup")]
        let endpoints = match &testing {
            Some(testing) => Endpoints {
                authorize: testing.base.join("oauth2/auth").unwrap(),
                token: testing.base.join("oauth2/token").unwrap(),
                revoke: testing.base.join("oauth2/revoke").unwrap(),
            },
            None => endpoints,
        };
        let login = Login::new(endpoints, CLIENT_ID, port);
        let page = login.authorize_url();
        let code = match &loopback {
            Some(loopback) => {
                if terminal.setup_guided() {
                    let progress = terminal.progress("Opening your browser...");
                    let opened = open_browser(&page).await;
                    drop(progress);
                    if !opened {
                        terminal.separator();
                        terminal.message("Couldn't open your browser. Open this page:");
                        terminal.message(page.as_str());
                    }
                } else {
                    terminal.message("Log in with Cloudflare in your browser. If it does not open, open this page:");
                    terminal.message(page.as_str());
                    open_browser(&page).await;
                }
                let _progress = terminal
                    .setup_guided()
                    .then(|| terminal.progress("Waiting for Cloudflare login..."));
                tokio::time::timeout(LOGIN_WAIT, loopback.callback(&login))
                    .await
                    .map_err(|_| {
                        CliError::new(
                            Code::AuthRequired,
                            "the login did not finish within 10 minutes",
                        )
                    })??
            }
            None => {
                if terminal.setup_guided() {
                    terminal.separator();
                }
                terminal.message(if terminal.setup_guided() {
                    "Open this page on any device:"
                } else {
                    "Open this page on any device, log in with Cloudflare and select Authorize:"
                });
                terminal.message(page.as_str());
                terminal.message(if terminal.setup_guided() {
                    "After you authorize, copy the address of the page that fails to load."
                } else {
                    "The browser then fails to load a page on 127.0.0.1. Copy that page's address."
                });
                let pasted = terminal
                    .input(
                        if terminal.setup_guided() {
                            "Callback address"
                        } else {
                            "Address"
                        },
                        true,
                    )
                    .await?;
                let url = Url::parse(&pasted)
                    .map_err(|error| CliError::usage(format!("that is not an address: {error}")))?;
                login.code_from(&url)?
            }
        };
        let _progress = terminal.setup_guided().then(|| {
            if args.no_browser {
                terminal.progress("Waiting for Cloudflare login...")
            } else {
                terminal.continue_progress("Waiting for Cloudflare login...")
            }
        });
        let grant = login.exchange(&http, code).await?;
        drop(_progress);
        let api_base = Url::parse(API_BASE).expect("the API base is a URL");
        #[cfg(feature = "test-setup")]
        let api_base = testing
            .map(|testing| testing.base.join("client/v4/").unwrap())
            .unwrap_or(api_base);
        Ok(Session {
            terminal,
            http,
            grant,
            api_base,
            account: Account {
                id: String::new(),
                name: String::new(),
            },
            subdomain: None,
        })
    }

    /// Reads the one account the user picked on the consent page.
    async fn pick_account(&mut self) -> Result<(), CliError> {
        let work = async {
            self.api()
                .accounts()
                .await
                .map_err(|error| CliError::from(error).context("cannot list the account"))
        };
        let accounts = if self.terminal.setup_guided() {
            let _progress = self
                .terminal
                .continue_progress("Waiting for Cloudflare login...");
            work.await?
        } else {
            self.terminal
                .wait("Checking your Cloudflare account...", work)
                .await?
        };
        match <[Account; 1]>::try_from(accounts) {
            Ok([account]) => self.account = account,
            Err(accounts) => {
                return Err(format!(
                    "the login gave access to {} accounts; pick exactly one on the consent page",
                    accounts.len()
                )
                .into());
            }
        }
        if self.terminal.setup_guided() {
            self.terminal
                .complete(&format!("Signed in to {}", self.account.name));
        } else {
            self.terminal.message(&format!(
                "Logged in to the Cloudflare account {}.",
                printable(&self.account.name)
            ));
        }
        Ok(())
    }

    /// Revokes the grant, retrying three times. A failure leaves the grant active until its tokens expire, so the user
    /// is told where to revoke it.
    async fn end(self) {
        self.terminal.cancel().await;
        for attempt in 0..4 {
            match self.grant.revoke(&self.http).await {
                Ok(()) => return,
                Err(error) if attempt == 3 => {
                    eprintln!(
                        "koi: could not revoke the Cloudflare login ({error}). It stays active; its access token expires within an hour. Revoke it now under Manage OAuth authorizations in the Cloudflare dashboard."
                    );
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    }

    fn api(&self) -> Cloudflare {
        Cloudflare::new(
            self.http.clone(),
            self.api_base.clone(),
            self.grant.access_token().clone(),
        )
    }

    async fn subdomain(&mut self) -> Result<String, CliError> {
        if let Some(subdomain) = &self.subdomain {
            return Ok(subdomain.clone());
        }
        let subdomain = self
            .api()
            .workers_subdomain(self.account())
            .await
            .map_err(|error| {
                CliError::from(error).context("cannot read the account's workers.dev subdomain")
            })?;
        self.subdomain = Some(subdomain.clone());
        Ok(subdomain)
    }

    async fn worker_url(&mut self, script: &str) -> Result<Url, CliError> {
        let subdomain = self.subdomain().await?;
        #[cfg(feature = "test-setup")]
        if let Some(testing) = testing::config()? {
            return Ok(testing.base.join(&format!("workers/{script}/")).unwrap());
        }
        Url::parse(&format!("https://{script}.{subdomain}.workers.dev/")).map_err(|error| {
            CliError::from(format!(
                "{script} does not make a workers.dev address: {error}"
            ))
        })
    }

    /// Waits until R2 is enabled on the account. R2 cannot be enabled through the API, so the user completes the
    /// dashboard's checkout (section 4.1).
    async fn require_r2(&mut self) -> Result<(), CliError> {
        let started = Instant::now();
        let mut told = false;
        let terminal = self.terminal.clone();
        let mut progress = terminal
            .setup_guided()
            .then(|| terminal.continue_progress("Checking storage..."));
        loop {
            match self
                .fresh()
                .await?
                .buckets(self.account(), Jurisdiction::Default)
                .await
            {
                Ok(_) => {
                    if told && let Some(progress) = progress.take() {
                        progress.finish("R2 is ready");
                    }
                    return Ok(());
                }
                Err(error) if error.has_code(R2_DISABLED) => {
                    if !told {
                        let page = Url::parse(&format!(
                            "https://dash.cloudflare.com/{}/r2/overview",
                            self.account()
                        ))
                        .expect("the R2 page is a URL");
                        if self.terminal.setup_guided() {
                            drop(progress.take());
                            terminal.separator();
                            self.terminal
                                .message("Complete the R2 checkout in your browser to continue.");
                            let opening = terminal.progress("Opening your browser...");
                            let opened = open_browser(&page).await;
                            drop(opening);
                            if !opened {
                                self.terminal
                                    .message("Couldn't open your browser. Open this page:");
                                self.terminal.message(page.as_str());
                            }
                            progress = Some(terminal.progress("Waiting for R2..."));
                        } else {
                            println!(
                                "R2 is not enabled on this account. Koidrive keeps your files in R2."
                            );
                            println!(
                                "Enable it on this page. Its checkout includes free monthly usage and bills use beyond it:\n{page}"
                            );
                            open_browser(&page).await;
                            println!("Waiting for R2...");
                        }
                        told = true;
                    }
                    if started.elapsed() > R2_WAIT {
                        return Err("R2 was not enabled within 30 minutes; run the command again once it is".into());
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(error) => {
                    return Err(CliError::caused(
                        format!("cannot list the account's R2 buckets: {error}"),
                        error,
                    ));
                }
            }
        }
    }

    /// The Koidrive servers in the account (section 6): scripts with the tag, and untagged scripts that bind the
    /// Koidrive Durable Object classes, each checked with `GET /v1/server`. A tagged script that binds a bucket but does
    /// not answer is a deploy that stopped halfway, and is kept without `info`.
    async fn find_servers(&mut self) -> Result<Vec<Found>, CliError> {
        let binding = embedded_bundle()?.manifest.r2_binding;
        let api = self.fresh().await?;
        let scripts = api
            .scripts(self.account())
            .await
            .map_err(|error| CliError::from(error).context("cannot list the account's Workers"))?;
        let mut found = Vec::new();
        for script in scripts {
            let settings = api
                .script_settings(self.account(), &script.id)
                .await
                .map_err(|error| {
                    CliError::caused(
                        format!("cannot read the settings of {}: {error}", script.id),
                        error,
                    )
                })?;
            let tagged = script.tags.iter().any(|tag| tag == TAG);
            let binds = |class: &str| {
                settings.bindings.iter().any(|bound| {
                    bound.kind == "durable_object_namespace"
                        && bound.class_name.as_deref() == Some(class)
                })
            };
            if !tagged && !SERVER_CLASSES.iter().all(|class| binds(class)) {
                continue;
            }
            let url = self.worker_url(&script.id).await?;
            let info = server_info(&url).await;
            let bucket = settings.r2_bucket(&binding).and_then(|bound| {
                Some(Bound {
                    name: bound.bucket_name.clone()?,
                    jurisdiction: bound.jurisdiction.unwrap_or_default(),
                })
            });
            if info.is_some() || (tagged && bucket.is_some()) {
                found.push(Found {
                    script,
                    bucket,
                    url,
                    info,
                });
            }
        }
        Ok(found)
    }

    /// The one server a command acts on: the named one, or the only one.
    async fn pick(&mut self, name: Option<&str>) -> Result<Found, CliError> {
        let mut found = self.find_servers().await?;
        if let Some(name) = name {
            found.retain(|server| server.script.id == name);
        }
        match found.len() {
            0 => Err(CliError::from(match name {
                Some(name) => format!("the account has no Koidrive server named {name}"),
                None => "the account has no Koidrive server; run koi server deploy".into(),
            })),
            1 => Ok(found.remove(0)),
            _ => Err(format!(
                "the account has several Koidrive servers; name one with --worker: {}",
                found
                    .iter()
                    .map(|server| server.script.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into()),
        }
    }

    /// The state file of an unfinished deploy in this account, if there is one. Any other run that has not finished
    /// stops the command here, before it calls the account: its file is that run's only record of what it created.
    fn resumable(&self, paths: &Paths) -> Result<Option<SetupState>, CliError> {
        let file = StateFile::new(paths);
        match file.load()? {
            Some(state) if self.continues(&state) => Ok(Some(state)),
            Some(other) => Err(unfinished(&other, self.account(), file.path())),
            None => Ok(None),
        }
    }

    /// Whether `state` is an unfinished deploy in this session's account, which setup goes on with.
    fn continues(&self, state: &SetupState) -> bool {
        state.account == self.account() && state.step != Step::Uninstalling
    }

    /// `koi server deploy` after the login: finish an interrupted deploy, or deploy a new server into an account
    /// that has none.
    async fn server_deploy(&mut self, paths: &Paths, device_name: &str) -> Result<(), CliError> {
        if let Some(state) = self.resumable(paths)? {
            return self.finish_deploy(paths, state, device_name).await;
        }
        crate::telemetry::step(SetupStep::R2Checkout, self.require_r2()).await?;
        let found = self.find_servers().await?;
        if let Some(server) = found.first() {
            return Err(format!(
                "the account already has a Koidrive server, {} at {}; run koi server add-device",
                server.script.id, server.url
            )
            .into());
        }
        self.deploy(paths, device_name).await
    }

    /// `koi server add-device` after the login: add this device to the named server, or to the only one.
    async fn server_add_device(
        &mut self,
        paths: &Paths,
        server: Option<&str>,
        device_name: &str,
    ) -> Result<(), CliError> {
        if let (Some(state), Some(name)) = (self.resumable(paths)?, server)
            && state.script != name
        {
            return Err(unfinished(
                &state,
                self.account(),
                StateFile::new(paths).path(),
            ));
        }
        let found = self.pick(server).await?;
        self.add_device(paths, found, device_name, false).await
    }

    /// `koi setup` after the login: finish an interrupted deploy, deploy a new server, or add this device.
    async fn setup(&mut self, paths: &Paths, device_name: &str) -> Result<(), CliError> {
        if let Some(state) = self.resumable(paths)? {
            self.terminal.message("Continuing your server setup.");
            return self.finish_deploy(paths, state, device_name).await;
        }
        crate::telemetry::step(SetupStep::R2Checkout, self.require_r2()).await?;
        let terminal = self.terminal.clone();
        let progress = terminal.progress("Looking for your server...");
        let mut found = self.find_servers().await?;
        let server = match found.len() {
            0 => {
                drop(progress);
                return self.deploy(paths, device_name).await;
            }
            1 => {
                progress.finish("Found your server");
                found.remove(0)
            }
            _ => {
                drop(progress);
                let choices = found
                    .iter()
                    .map(|server| {
                        server
                            .url
                            .host_str()
                            .unwrap_or(server.script.id.as_str())
                            .to_owned()
                    })
                    .collect();
                let index = self
                    .terminal
                    .select("Which server should this device use?", choices)
                    .await?;
                found.remove(index)
            }
        };
        self.add_device(paths, server, device_name, true).await
    }

    /// Picks a free Worker name and a new bucket name, then deploys and claims a new server (sections 4.3 and 4.4).
    async fn deploy(&mut self, paths: &Paths, device_name: &str) -> Result<(), CliError> {
        let terminal = self.terminal.clone();
        let progress = terminal
            .setup_guided()
            .then(|| terminal.continue_progress("Preparing storage..."));
        let bundle = embedded_bundle()?;
        let api = self.fresh().await?;
        let scripts: Vec<String> = api
            .scripts(self.account())
            .await
            .map_err(|error| CliError::from(error).context("cannot list the account's Workers"))?
            .into_iter()
            .map(|script| script.id)
            .collect();
        let base = &bundle.manifest.default_bucket;
        let state = SetupState {
            account: self.account().to_owned(),
            script: free_name(base, &scripts),
            bucket: new_bucket_name(base),
            ..SetupState::default()
        };
        StateFile::new(paths).save(&state)?;
        drop(progress);
        self.finish_deploy(paths, state, device_name).await
    }

    /// Runs the deploy steps after `state.step` and claims the server. Every step is safe to repeat.
    async fn finish_deploy(
        &mut self,
        paths: &Paths,
        mut state: SetupState,
        device_name: &str,
    ) -> Result<(), CliError> {
        let file = StateFile::new(paths);
        let bundle = embedded_bundle()?;
        let mut key = None;
        let terminal = self.terminal.clone();
        let progress = terminal
            .setup_guided()
            .then(|| terminal.progress("Preparing storage..."));
        if state.step < Step::BucketCreated {
            if !terminal.setup_guided() {
                println!("Creating the R2 bucket {}...", state.bucket);
            }
            crate::telemetry::step(SetupStep::Bucket, create_bucket(self, &file, &mut state))
                .await?;
        }
        if state.step < Step::ScriptUploaded {
            keep_script_name(
                self,
                &mut state,
                &bundle.manifest.r2_binding,
                &bundle.manifest.default_bucket,
            )
            .await?;
            if let Some(progress) = &progress {
                progress.update("Deploying your server...");
            } else {
                println!(
                    "Uploading Koidrive {} as the Worker {}...",
                    bundle.manifest.version, state.script
                );
            }
            let secret = Secret::random(32);
            state.secret_set = true;
            file.save(&state)?;
            let bucket = Bound {
                name: state.bucket.clone(),
                jurisdiction: state.jurisdiction,
            };
            let upload = self.upload(&bundle, &state.script, &bucket, None, Some(&secret));
            crate::telemetry::step(SetupStep::Script, upload).await?;
            state.step = Step::ScriptUploaded;
            file.save(&state)?;
            key = Some(secret);
        }
        let url = self.worker_url(&state.script).await?;
        if let Some(progress) = &progress {
            progress.update("Waiting for your server...");
        }
        if state.step < Step::Serving {
            crate::telemetry::step(SetupStep::Serving, self.serve(&state.script, &url)).await?;
            state.step = Step::Serving;
            file.save(&state)?;
        }
        let server = Found {
            script: Script {
                id: state.script.clone(),
                created_on: None,
                tags: vec![TAG.to_owned()],
                migration_tag: None,
                has_assets: true,
            },
            bucket: Some(Bound {
                name: state.bucket.clone(),
                jurisdiction: state.jurisdiction,
            }),
            info: server_info(&url).await,
            url,
        };
        if let Some(progress) = progress {
            progress.finish("Server created");
        }
        let _progress = terminal
            .setup_guided()
            .then(|| terminal.progress("Connecting this device..."));
        let claim =
            Claim::new(self, paths, &server, device_name, state, key).run(Duration::from_secs(60));
        crate::telemetry::step(SetupStep::Claim, claim).await
    }

    /// Adds this device to a server that already exists (section 4.5). `interactive` lets it finish a deploy that
    /// stopped before the server answered, and offer an upgrade when the server's claim code does not handle setup
    /// attempts; otherwise it stops and names the command that does.
    async fn add_device(
        &mut self,
        paths: &Paths,
        mut server: Found,
        device_name: &str,
        interactive: bool,
    ) -> Result<(), CliError> {
        let file = StateFile::new(paths);
        let saved = match file.load()? {
            Some(state) if self.continues(&state) && state.script == server.script.id => {
                Some(state)
            }
            Some(other) => return Err(unfinished(&other, self.account(), file.path())),
            None => None,
        };
        let info = match server.info.clone() {
            Some(info) => info,
            None if interactive => {
                let terminal = self.terminal.clone();
                let progress = if terminal.setup_guided() {
                    Some(terminal.progress("Finishing your server setup..."))
                } else {
                    println!(
                        "{} stopped halfway through its deploy. Finishing it...",
                        server.script.id
                    );
                    None
                };
                let info = self.serve(&server.script.id, &server.url).await?;
                if let Some(progress) = progress {
                    progress.finish("Server is ready");
                }
                server.info = Some(info.clone());
                info
            }
            None => {
                return Err(format!(
                    "{} stopped halfway through its deploy; run koi setup to finish it",
                    server.script.id
                )
                .into());
            }
        };
        let mut window = Duration::from_secs(60);
        if !info.setup_attempts {
            let reason = if self.terminal.setup_guided() {
                format!(
                    "Your server needs Koidrive {} before it can connect this device.",
                    embedded_bundle()?.manifest.version
                )
            } else {
                format!(
                    "{} runs Koidrive {}, which cannot make a retried claim end with one device. It needs an upgrade to {} first.",
                    server.script.id,
                    info.server_version,
                    embedded_bundle()?.manifest.version
                )
            };
            if !interactive {
                return Err(
                    format!("{reason} Run koi server upgrade, then this command again.").into(),
                );
            }
            if let Err(error) = confirm(
                &self.terminal,
                &format!("{reason} Upgrade it now?"),
                false,
                "--yes",
            )
            .await
            {
                if error.code == Code::Interrupted {
                    return Err(error);
                }
                return Err(
                    format!("{reason} Run koi server upgrade, then koi setup again.").into(),
                );
            }
            self.upgrade(&server, false).await?;
            let terminal = self.terminal.clone();
            let _progress = if terminal.setup_guided() {
                Some(terminal.progress("Waiting for your server..."))
            } else {
                println!(
                    "Waiting for the server's storage to move to the new version (up to 330 seconds)..."
                );
                None
            };
            wait_for_setup_attempts(&server.url).await?;
            window = Duration::from_secs(330);
        }
        if !info.claimed {
            window = Duration::from_secs(330);
        }
        let state = match saved {
            Some(state) => state,
            None => SetupState {
                account: self.account().to_owned(),
                script: server.script.id.clone(),
                bucket: server
                    .bucket
                    .as_ref()
                    .map(|bucket| bucket.name.clone())
                    .unwrap_or_default(),
                jurisdiction: server
                    .bucket
                    .as_ref()
                    .map(|bucket| bucket.jurisdiction)
                    .unwrap_or_default(),
                step: Step::Serving,
                ..SetupState::default()
            },
        };
        let terminal = self.terminal.clone();
        let _progress = terminal
            .setup_guided()
            .then(|| terminal.progress("Connecting this device..."));
        let claim = Claim::new(self, paths, &server, device_name, state, None).run(window);
        crate::telemetry::step(SetupStep::Claim, claim).await
    }

    /// Turns on the Worker's workers.dev route and waits until the server answers there.
    async fn serve(&mut self, script: &str, url: &Url) -> Result<ServerInfo, CliError> {
        self.fresh()
            .await?
            .enable_workers_dev(self.account(), script)
            .await
            .map_err(|error| {
                CliError::from(error).context(format!("cannot turn on workers.dev for {script}"))
            })?;
        wait_until_serving(url, None).await
    }

    /// Uploads the embedded server over an existing one (section 5).
    async fn upgrade(&mut self, server: &Found, yes: bool) -> Result<(), CliError> {
        let bundle = embedded_bundle()?;
        let Some(info) = &server.info else {
            return Err(format!(
                "{} does not answer; run koi setup to finish its deploy",
                server.script.id
            )
            .into());
        };
        if newer(&info.server_version, &bundle.manifest.version) {
            return Err(format!(
                "{} runs Koidrive {}, newer than this CLI's {}; install a newer koi",
                server.script.id, info.server_version, bundle.manifest.version
            )
            .into());
        }
        if !server.script.tags.iter().any(|tag| tag == TAG) {
            confirm(
                &self.terminal,
                &format!(
                    "{} was deployed with the deploy button. A push to the repository the button created deploys that repository's code again and replaces this upgrade. Update that repository too, or disconnect its builds in the dashboard. Upgrade now?",
                    server.script.id
                ),
                yes,
                "--yes",
            ).await?;
        }
        let bucket = server
            .bucket
            .clone()
            .ok_or_else(|| format!("{} binds no R2 bucket", server.script.id))?;
        let terminal = self.terminal.clone();
        let progress = if terminal.setup_guided() {
            Some(terminal.progress("Updating your server..."))
        } else {
            println!(
                "Uploading Koidrive {} over {} {}...",
                bundle.manifest.version, server.script.id, info.server_version
            );
            None
        };
        self.upload(
            &bundle,
            &server.script.id,
            &bucket,
            server.script.migration_tag.as_deref(),
            None,
        )
        .await?;
        wait_until_serving(&server.url, Some(&bundle.manifest.version)).await?;
        if let Some(progress) = progress {
            progress.finish("Server updated");
        } else {
            println!(
                "{} now runs Koidrive {}. Open sync sessions move to it within 5 minutes.",
                server.script.id, bundle.manifest.version
            );
        }
        Ok(())
    }

    async fn upload(
        &mut self,
        bundle: &Bundle,
        script: &str,
        bucket: &Bound,
        deployed_tag: Option<&str>,
        secret: Option<&Secret>,
    ) -> Result<(), CliError> {
        let api = self.fresh().await?;
        let jwt = api
            .upload_assets(self.account(), script, &bundle.assets)
            .await
            .map_err(|error| CliError::from(error).context("cannot upload the static assets"))?;
        let plan = UploadPlan::new(
            bundle,
            &bucket.name,
            bucket.jurisdiction,
            deployed_tag,
            secret,
            Some(&jwt),
        )
        .map_err(|error| error.to_string())?;
        api.upload_script(self.account(), script, &plan)
            .await
            .map_err(|error| {
                CliError::from(error).context(format!("cannot upload the Worker {script}"))
            })?;
        Ok(())
    }

    /// Deletes the Worker, then empties and deletes its bucket unless another Worker or application uses it (section 5).
    /// A state file of an unfinished uninstall in this account continues it, unless `bucket` names one.
    async fn uninstall(
        &mut self,
        paths: &Paths,
        name: Option<&str>,
        bucket: Option<&str>,
        jurisdiction: Jurisdiction,
        yes: bool,
    ) -> Result<(), CliError> {
        let file = StateFile::new(paths);
        match file.load()? {
            Some(state)
                if state.account == self.account()
                    && state.step == Step::Uninstalling
                    && bucket.is_none() =>
            {
                println!(
                    "Continuing the uninstall that stopped earlier ({}).",
                    state.bucket
                );
                let script = state.script.clone();
                uninstall::finish(self, &file, state).await?;
                self.forget(paths, &script).await;
                return Ok(());
            }
            Some(other) => return Err(unfinished(&other, self.account(), file.path())),
            None => {}
        }
        let (state, contents) = match bucket {
            Some(bucket) => uninstall::plan(self, None, bucket, jurisdiction).await?,
            None => {
                let server = self.pick(name).await?;
                let bound = server
                    .bucket
                    .clone()
                    .ok_or_else(|| format!("{} binds no R2 bucket", server.script.id))?;
                uninstall::plan(self, Some(&server.script), &bound.name, bound.jurisdiction).await?
            }
        };
        let worker = match state.script.as_str() {
            "" => String::new(),
            script => format!("the Worker {script} and "),
        };
        confirm(
            &self.terminal,
            &format!(
                "Delete {worker}the R2 bucket {} in the {} jurisdiction with its {} objects? Every file in the bucket is lost, also anything another application stored there through R2's S3 API.",
                state.bucket,
                state.jurisdiction.as_str(),
                contents.objects
            ),
            yes,
            "--yes",
        ).await?;
        file.save(&state)?;
        let script = state.script.clone();
        uninstall::finish(self, &file, state).await?;
        self.forget(paths, &script).await;
        Ok(())
    }

    /// Stops and removes this device's background service when the deleted Worker `script` was the server it syncs
    /// with, at its workers.dev address.
    async fn forget(&mut self, paths: &Paths, script: &str) {
        if script.is_empty() {
            return;
        }
        let url = match self.worker_url(script).await {
            Ok(url) => url,
            Err(error) => {
                println!(
                    "Couldn't check whether this device synced with the deleted server: {}. If it did, unlink its folders with koi folder unlink, which also removes the background service.",
                    printable(error.message.trim_end_matches('.'))
                );
                return;
            }
        };
        if let Some(report) = crate::service::forget(paths, &url).await {
            print!("{}", report.text());
        }
    }
}

/// Fails when this koi has no embedded server bundle. The commands that find, deploy or upgrade a server call it before
/// the Cloudflare login, so that nobody authorizes a login that cannot be used.
fn bundled() -> Result<(), String> {
    #[cfg(feature = "test-setup")]
    if testing::config()?.is_some() {
        return Ok(());
    }
    if SERVER_BUNDLE.is_empty() {
        return Err("this koi was built without a server bundle, so it cannot find, deploy or upgrade a server; build it with KOIDRIVE_SERVER_BUNDLE as docs/guide/install.md describes".into());
    }
    Ok(())
}

/// The embedded server bundle, read through [`Bundle::from_dir`] from a private copy, so the release's bundle passes the
/// same checks as one on disk.
pub(crate) fn embedded_bundle() -> Result<Bundle, CliError> {
    #[cfg(feature = "test-setup")]
    if let Some(testing) = testing::config()? {
        return Bundle::from_dir(&testing.bundle).map_err(|error| error.to_string().into());
    }
    bundled()?;
    let broken = |error: &dyn std::fmt::Display| {
        CliError::from(format!("this koi's server bundle is broken: {error}"))
    };
    let dir = tempfile::tempdir()
        .map_err(|error| CliError::from(format!("cannot unpack the server bundle: {error}")))?;
    for (path, bytes) in SERVER_BUNDLE {
        let target = dir.path().join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| broken(&error))?;
        }
        std::fs::write(&target, bytes).map_err(|error| broken(&error))?;
    }
    Bundle::from_dir(dir.path()).map_err(|error| broken(&error))
}

/// Creates the bucket `state.bucket` and records the step. `deploy` saved that name, which only this deploy produces, before
/// the first try, so a 409 for it means an earlier try of this deploy created the bucket and its answer was lost.
async fn create_bucket(
    cloud: &mut impl Cloud,
    file: &StateFile,
    state: &mut SetupState,
) -> Result<(), CliError> {
    let account = cloud.account().to_owned();
    match cloud
        .fresh()
        .await?
        .create_bucket(&account, &state.bucket, state.jurisdiction)
        .await
    {
        Ok(()) => {}
        Err(error) if error.status() == Some(409) => {}
        Err(error) => {
            return Err(CliError::caused(
                format!("cannot create the bucket {}: {error}", state.bucket),
                error,
            ));
        }
    }
    state.step = Step::BucketCreated;
    Ok(file.save(state)?)
}

/// A bucket name only this deploy produces: `base`, a hyphen and 16 random hex digits (64 bits). Another
/// application cannot hold it unless it guesses it, so the deploy never takes over someone else's bucket.
fn new_bucket_name(base: &str) -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the operating system provides random bytes");
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{base}-{suffix}")
}

/// Keeps `state.script` while no other application holds the name. A Worker under it that has the tag and binds
/// `state.bucket` is this deploy's own upload from before the state file recorded it, so the upload goes over it. Any
/// other Worker under the name belongs to someone else, so the deploy moves to the first free name after `base`.
async fn keep_script_name(
    cloud: &mut impl Cloud,
    state: &mut SetupState,
    binding: &str,
    base: &str,
) -> Result<(), CliError> {
    let api = cloud.fresh().await?;
    let account = cloud.account().to_owned();
    let scripts = api
        .scripts(&account)
        .await
        .map_err(|error| CliError::from(error).context("cannot list the account's Workers"))?;
    let Some(current) = scripts.iter().find(|script| script.id == state.script) else {
        return Ok(());
    };
    if current.tags.iter().any(|tag| tag == TAG) {
        let settings = api
            .script_settings(&account, &state.script)
            .await
            .map_err(|error| {
                CliError::caused(
                    format!("cannot read the settings of {}: {error}", state.script),
                    error,
                )
            })?;
        if settings.r2_bucket(binding).is_some_and(|bound| {
            bound.bucket_name.as_deref() == Some(state.bucket.as_str())
                && bound.jurisdiction.unwrap_or_default() == state.jurisdiction
        }) {
            return Ok(());
        }
    }
    let taken: Vec<String> = scripts.into_iter().map(|script| script.id).collect();
    let free = free_name(base, &taken);
    println!(
        "Another application now has a Worker named {}, so the server becomes the Worker {free}.",
        state.script
    );
    state.script = free;
    Ok(())
}

/// `base`, or the first of `base-2`, `base-3`, ... that is not taken.
fn free_name(base: &str, taken: &[String]) -> String {
    std::iter::once(base.to_owned())
        .chain((2..).map(|n| format!("{base}-{n}")))
        .find(|name| !taken.contains(name))
        .expect("some name is free")
}

fn newer(a: &str, b: &str) -> bool {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(a), Ok(b)) => a.cmp_precedence(&b).is_gt(),
        _ => false,
    }
}

/// The Koidrive API of the server at `url`. Tests serve it over http on a loopback address.
fn server_api(url: &Url) -> Result<Api, ApiError> {
    #[cfg(feature = "test-setup")]
    if let Some(testing) = testing::config().map_err(ApiError::Url)?
        && url.origin() == testing.base.origin()
    {
        return Api::local_development(url.clone());
    }
    if cfg!(test) {
        Api::local_development(url.clone())
    } else {
        Api::new(url.clone())
    }
}

async fn server_info(url: &Url) -> Option<ServerInfo> {
    let api = server_api(url).ok()?;
    tokio::time::timeout(Duration::from_secs(10), api.server_info())
        .await
        .ok()?
        .ok()
}

/// Polls `GET /v1/server` until it answers, and until it reports `version` when one is given.
async fn wait_until_serving(url: &Url, version: Option<&str>) -> Result<ServerInfo, CliError> {
    let started = Instant::now();
    loop {
        if let Some(info) = server_info(url)
            .await
            .filter(|info| version.is_none_or(|version| info.server_version == version))
        {
            return Ok(info);
        }
        if started.elapsed() > SERVE_WAIT {
            return Err(format!(
                "{url} did not answer as expected within {} seconds",
                SERVE_WAIT.as_secs()
            )
            .into());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Polls until the Account DO reports `setup_attempts`, which can take up to 300 seconds after an upgrade.
async fn wait_for_setup_attempts(url: &Url) -> Result<(), CliError> {
    let started = Instant::now();
    loop {
        if server_info(url)
            .await
            .is_some_and(|info| info.setup_attempts)
        {
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(330) {
            return Err("the server did not move to the new version within 330 seconds; run the command again".into());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use koidrive_proto::v1::DeviceCredentials;
    use serde_json::json;

    use super::fake::{self, ACCOUNT, Fake, Reply, cf, proto, revoked, tokens};
    use super::*;

    #[test]
    fn a_closed_input_is_not_an_empty_answer() {
        let closed = read_line(&b""[..]).unwrap_err();
        assert_eq!(closed.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(read_line(&b"\n"[..]).unwrap(), "\n");
        assert_eq!(read_line(&b"n\n"[..]).unwrap(), "n\n");
    }

    #[test]
    fn setup_guides_only_on_two_terminals_without_json_or_ci() {
        let guided = |stdin, stderr, json, ci| mode(stdin, stderr, json, ci, false).unwrap();
        assert_eq!(guided(true, true, false, false), Mode::Guided);
        for (stdin, stderr, json, ci) in [
            (false, true, false, false),
            (true, false, false, false),
            (false, false, false, false),
            (true, true, true, false),
            (true, true, false, true),
            (true, true, true, true),
        ] {
            assert_eq!(
                guided(stdin, stderr, json, ci),
                Mode::Direct,
                "stdin {stdin}, stderr {stderr}, json {json}, CI {ci}"
            );
        }
    }

    #[test]
    fn interactive_asks_for_the_guide_and_fails_without_terminals() {
        assert_eq!(mode(true, true, false, true, true).unwrap(), Mode::Guided);
        for (stdin, stderr) in [(false, true), (true, false), (false, false)] {
            let error = mode(stdin, stderr, false, false, true).unwrap_err();
            assert_eq!(error.code, Code::Usage);
            assert!(error.message.contains("needs a terminal"), "{error:?}");
        }
        assert_eq!(
            mode(true, true, true, false, true).unwrap_err().code,
            Code::Usage
        );
    }

    #[test]
    fn setup_lists_every_way_in_with_its_argv() {
        let argv: Vec<String> = choices()
            .iter()
            .map(|choice| choice.argv.join(" "))
            .collect();
        assert_eq!(
            argv,
            [
                "koi join <CODE>",
                "koi server deploy --interactive",
                "koi server add-device --interactive",
                "koi server claim <SERVER_URL> <CODE> --user <NAME>",
                "koi setup --interactive",
            ]
        );
    }

    #[tokio::test]
    async fn json_and_a_pasted_login_stop_before_the_login_and_name_the_interactive_command() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let parse = |args: &[&str]| match crate::Cli::try_parse_from(args).unwrap().command {
            Some(crate::Command::Server { action }) => action,
            _ => unreachable!(),
        };

        let error = server(
            &paths,
            parse(&["koi", "server", "deploy"]),
            true,
            Arc::new(Terminal::new(true, false)),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, Code::AuthRequired);
        assert_eq!(
            error.next,
            [Next::of(&["koi", "server", "deploy", "--interactive"])]
        );

        let action = parse(&["koi", "server", "upgrade", "--worker", "fb", "--no-browser"]);
        let error = server(&paths, action, false, Arc::new(Terminal::new(true, false)))
            .await
            .unwrap_err();
        assert_eq!(error.code, Code::AuthRequired);
        assert_eq!(
            error.next,
            [Next::of(&[
                "koi",
                "server",
                "upgrade",
                "--worker",
                "fb",
                "--interactive",
                "--no-browser"
            ])]
        );
    }

    #[tokio::test]
    async fn owner_commands_without_interactive_and_terminals_need_auth_and_name_the_interactive_command()
     {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let parse = |args: &[&str]| match crate::Cli::try_parse_from(args).unwrap().command {
            Some(crate::Command::Server { action }) => action,
            _ => unreachable!(),
        };
        for (args, next) in [
            (&["deploy"][..], &["deploy", "--interactive"][..]),
            (&["add-device"], &["add-device", "--interactive"]),
            (
                &["upgrade", "--worker", "fb", "--yes"],
                &["upgrade", "--worker", "fb", "--interactive"],
            ),
            (&["upgrade"], &["upgrade", "--interactive"]),
            (
                &["delete", "--bucket", "fb-1a2b", "--yes"],
                &["delete", "--bucket", "fb-1a2b", "--interactive"],
            ),
            (&["deploy", "--interactive"], &["deploy", "--interactive"]),
        ] {
            let argv = [&["koi", "server"][..], args].concat();
            let error = server(
                &paths,
                parse(&argv),
                false,
                Arc::new(Terminal::new(true, false)),
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, Code::AuthRequired, "{args:?}");
            assert_eq!(
                error.next,
                [Next::of(&[&["koi", "server"][..], next].concat())],
                "{args:?}"
            );
        }
        assert!(!dir.path().join("state").exists(), "nothing was set up");
    }

    fn deployed(fake: &Fake) -> Found {
        Found {
            script: Script {
                id: "koidrive".into(),
                created_on: None,
                tags: vec![TAG.into()],
                migration_tag: None,
                has_assets: true,
            },
            bucket: Some(Bound {
                name: "koidrive".into(),
                jurisdiction: Jurisdiction::Default,
            }),
            url: fake.base.clone(),
            info: None,
        }
    }

    #[test]
    fn a_stop_ends_a_prompt_nobody_answers_and_the_runtime_does_not_wait_for_it() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime.block_on(guarded(
            async {
                read_answer(|| {
                    loop {
                        std::thread::park();
                    }
                })
                .await?;
                unreachable!("nobody answers the prompt")
            },
            async || "stopped",
            async |_: &mut Session| Ok(()),
        ));
        assert_eq!(
            outcome.unwrap_err().message,
            "stopped before the login finished"
        );
        let (dropped, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(runtime);
            dropped.send(()).unwrap();
        });
        done.recv_timeout(Duration::from_secs(5))
            .expect("the runtime stops without waiting for the blocked read");
    }

    #[tokio::test]
    async fn an_answer_is_read_without_its_line_end() {
        let answer = read_answer(|| Ok(" y\n".to_owned())).await;
        assert_eq!(answer.unwrap(), "y");
        let failed = read_answer(|| Err(std::io::Error::other("closed"))).await;
        assert_eq!(failed.unwrap_err().message, "closed");
    }

    #[tokio::test]
    async fn a_stop_during_the_account_lookup_still_revokes_the_grant() {
        let oauth = Fake::start(vec![tokens(), revoked(), revoked()]).await;
        let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base =
            Url::parse(&format!("http://{}/client/v4/", api.local_addr().unwrap())).unwrap();
        let mut held = Vec::new();
        let stopped = async || {
            held.push(api.accept().await.unwrap().0);
            "stopped"
        };

        let outcome = guarded(
            async { Ok(fake::login(&oauth, api_base).await) },
            stopped,
            async |_: &mut Session| Ok(()),
        )
        .await;

        assert_eq!(
            outcome.unwrap_err().message,
            "stopped; run the same command again to continue"
        );
        let revocations = oauth
            .seen()
            .iter()
            .filter(|seen| seen.path == "/oauth2/revoke")
            .count();
        assert_eq!(revocations, 2);
    }

    #[tokio::test]
    async fn a_resumed_deploy_moves_off_a_worker_name_another_application_took() {
        let binds = |bucket: &str| json!({ "bindings": [{ "type": "r2_bucket", "name": "BUCKET", "bucket_name": bucket }] });
        let cases = [
            (
                json!([{ "id": "koidrive", "tags": [] }]),
                None,
                "koidrive-2",
            ),
            (
                json!([{ "id": "koidrive", "tags": [TAG] }, { "id": "koidrive-2", "tags": [] }]),
                Some(binds("photos")),
                "koidrive-3",
            ),
            (
                json!([{ "id": "koidrive", "tags": [TAG] }]),
                Some(binds("koidrive")),
                "koidrive",
            ),
            (json!([{ "id": "photos", "tags": [] }]), None, "koidrive"),
        ];
        for (scripts, settings, expected) in cases {
            let mut replies = vec![tokens(), cf("GET", "/workers/scripts", scripts)];
            replies.extend(
                settings.map(|settings| cf("GET", "/workers/scripts/koidrive/settings", settings)),
            );
            let fake = Fake::start(replies).await;
            let mut session = fake::session(&fake).await;
            let mut state = SetupState {
                account: ACCOUNT.into(),
                script: "koidrive".into(),
                bucket: "koidrive".into(),
                step: Step::BucketCreated,
                ..SetupState::default()
            };

            keep_script_name(&mut session, &mut state, "BUCKET", "koidrive")
                .await
                .unwrap();

            assert_eq!(state.script, expected);
        }
    }

    #[test]
    fn a_deploy_names_its_bucket_with_a_random_suffix() {
        let first = new_bucket_name("koidrive");
        let suffix = first.strip_prefix("koidrive-").unwrap();
        assert_eq!(suffix.len(), 16);
        assert!(
            suffix
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert_ne!(first, new_bucket_name("koidrive"));
    }

    /// The bucket names of the creates `fake` received.
    fn created(fake: &Fake) -> Vec<String> {
        fake.seen()
            .iter()
            .filter(|seen| seen.method == "POST" && seen.path.ends_with("/r2/buckets"))
            .map(|seen| {
                let body: serde_json::Value = serde_json::from_slice(&seen.body).unwrap();
                body["name"].as_str().unwrap().to_owned()
            })
            .collect()
    }

    fn create_answer(status: u16) -> Reply {
        Reply {
            method: "POST",
            path: format!("/client/v4/accounts/{ACCOUNT}/r2/buckets"),
            status,
            content_type: "application/json",
            body: json!({ "success": status == 200, "errors": [], "result": {} })
                .to_string()
                .into_bytes(),
        }
    }

    #[tokio::test]
    async fn a_lost_create_answer_is_adopted_under_the_saved_name_on_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let file = StateFile::new(&Paths::under(dir.path()));
        let name = new_bucket_name("koidrive");
        let planned = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive".into(),
            bucket: name.clone(),
            ..SetupState::default()
        };
        file.save(&planned).unwrap();

        let lost = Fake::start(vec![tokens(), create_answer(502)]).await;
        let mut state = file.load().unwrap().unwrap();
        let error = create_bucket(&mut fake::session(&lost).await, &file, &mut state)
            .await
            .unwrap_err()
            .message;
        assert!(error.contains("cannot create the bucket"), "{error}");
        assert_eq!(file.load().unwrap(), Some(planned));

        let retry = Fake::start(vec![tokens(), create_answer(409)]).await;
        let mut state = file.load().unwrap().unwrap();
        create_bucket(&mut fake::session(&retry).await, &file, &mut state)
            .await
            .unwrap();

        assert_eq!(created(&lost), [name.as_str()]);
        assert_eq!(created(&retry), [name.as_str()]);
        let saved = file.load().unwrap().unwrap();
        assert_eq!((saved.bucket, saved.step), (name, Step::BucketCreated));
    }

    #[tokio::test]
    async fn a_bucket_named_like_the_default_is_never_created_read_or_deleted() {
        let fake = Fake::start(vec![tokens(), create_answer(200)]).await;
        let dir = tempfile::tempdir().unwrap();
        let file = StateFile::new(&Paths::under(dir.path()));
        let mut state = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive".into(),
            bucket: new_bucket_name("koidrive"),
            ..SetupState::default()
        };

        create_bucket(&mut fake::session(&fake).await, &file, &mut state)
            .await
            .unwrap();

        assert_eq!(created(&fake), [state.bucket.as_str()]);
        assert_ne!(state.bucket, "koidrive");
        assert!(
            !fake
                .seen()
                .iter()
                .any(|seen| seen.path.contains("/r2/buckets/koidrive") || seen.method == "DELETE")
        );
    }

    #[tokio::test]
    async fn setup_finishes_a_deploy_that_stopped_before_the_server_answered() {
        let info = ServerInfo {
            server_version: "0.1.0".into(),
            setup_attempts: true,
            ..ServerInfo::default()
        };
        let issued = DeviceCredentials {
            user_id: "user".into(),
            device_id: "0194f8a2-0000-7000-8000-000000000002".into(),
            refresh_credential: "device".into(),
            ..DeviceCredentials::default()
        };
        let secrets = "/workers/scripts/koidrive/secrets";
        let fake = Fake::start(vec![
            tokens(),
            cf("POST", "/workers/scripts/koidrive/subdomain", json!(null)),
            proto("GET", "/v1/server", info),
            cf("GET", secrets, json!([])),
            cf("PUT", secrets, json!(null)),
            proto("POST", "/v1/claim", issued),
            cf(
                "DELETE",
                &format!("{secrets}/KOIDRIVE_RECOVERY_KEY"),
                json!(null),
            ),
            cf("GET", secrets, json!([])),
        ])
        .await;
        let mut session = fake::session(&fake).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());

        session
            .add_device(&paths, deployed(&fake), "laptop", true)
            .await
            .unwrap();

        assert!(fake.seen().iter().any(|seen| seen.method == "POST"
            && seen.path.ends_with("/workers/scripts/koidrive/subdomain")));
        assert!(paths.credential().exists());
        assert_eq!(StateFile::new(&paths).load().unwrap(), None);
    }

    #[tokio::test]
    async fn uninstall_stops_at_another_unfinished_run_before_calling_the_account() {
        let deploy = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive".into(),
            bucket: new_bucket_name("koidrive"),
            step: Step::BucketCreated,
            ..SetupState::default()
        };
        let uninstall = SetupState {
            step: Step::Uninstalling,
            ..deploy.clone()
        };
        let elsewhere = SetupState {
            account: "another-account".into(),
            ..uninstall.clone()
        };
        let bucket = new_bucket_name("koidrive");
        for (other, named) in [
            (deploy, None),
            (elsewhere, None),
            (uninstall, Some(bucket.as_str())),
        ] {
            let fake = Fake::start(vec![tokens()]).await;
            let mut session = fake::session(&fake).await;
            let before = fake.seen().len();
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            StateFile::new(&paths).save(&other).unwrap();

            let error = session
                .uninstall(&paths, None, named, Jurisdiction::Default, true)
                .await
                .unwrap_err()
                .message;

            assert!(error.contains("has not finished"), "{error}");
            assert_eq!(StateFile::new(&paths).load().unwrap(), Some(other));
            assert_eq!(fake.seen().len(), before, "{error}");
        }
    }

    #[tokio::test]
    async fn setup_and_deploy_stop_at_an_unfinished_run_before_calling_the_account() {
        let others = [
            SetupState {
                account: "another-account".into(),
                script: "koidrive".into(),
                bucket: new_bucket_name("koidrive"),
                step: Step::BucketCreated,
                ..SetupState::default()
            },
            SetupState {
                account: ACCOUNT.into(),
                script: "koidrive".into(),
                bucket: new_bucket_name("koidrive"),
                step: Step::Uninstalling,
                ..SetupState::default()
            },
        ];
        for other in others {
            for entry in ["setup", "server deploy", "server add-device"] {
                let fake = Fake::start(vec![tokens()]).await;
                let mut session = fake::session(&fake).await;
                let before = fake.seen().len();
                let dir = tempfile::tempdir().unwrap();
                let paths = Paths::under(dir.path());
                StateFile::new(&paths).save(&other).unwrap();

                let error = match entry {
                    "setup" => session.setup(&paths, "desk").await,
                    "server deploy" => session.server_deploy(&paths, "desk").await,
                    _ => session.server_add_device(&paths, None, "desk").await,
                }
                .unwrap_err()
                .message;

                assert!(error.contains("has not finished"), "{entry}: {error}");
                assert_eq!(StateFile::new(&paths).load().unwrap(), Some(other.clone()));
                assert_eq!(fake.seen().len(), before, "{entry} called the account");
            }
        }
    }

    #[tokio::test]
    async fn adding_a_device_keeps_the_unfinished_deploy_of_another_worker() {
        let fake = Fake::start(vec![tokens()]).await;
        let mut session = fake::session(&fake).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let unfinished = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive-2".into(),
            bucket: new_bucket_name("koidrive"),
            step: Step::BucketCreated,
            ..SetupState::default()
        };
        StateFile::new(&paths).save(&unfinished).unwrap();

        let error = session
            .add_device(&paths, deployed(&fake), "laptop", true)
            .await
            .unwrap_err()
            .message;

        assert!(
            error.contains("the deploy of koidrive-2 has not finished"),
            "{error}"
        );
        assert_eq!(StateFile::new(&paths).load().unwrap(), Some(unfinished));
        assert!(
            !fake
                .seen()
                .iter()
                .any(|seen| seen.path.contains("/accounts/") || seen.path.starts_with("/v1/"))
        );
    }

    #[tokio::test]
    async fn adding_a_device_to_a_named_server_stops_at_another_workers_deploy_before_calling_the_account()
     {
        let fake = Fake::start(vec![tokens()]).await;
        let mut session = fake::session(&fake).await;
        let before = fake.seen().len();
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let unfinished = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive-2".into(),
            bucket: new_bucket_name("koidrive"),
            step: Step::BucketCreated,
            ..SetupState::default()
        };
        StateFile::new(&paths).save(&unfinished).unwrap();

        let error = session
            .server_add_device(&paths, Some("koidrive"), "laptop")
            .await
            .unwrap_err()
            .message;

        assert!(
            error.contains("the deploy of koidrive-2 has not finished"),
            "{error}"
        );
        assert_eq!(StateFile::new(&paths).load().unwrap(), Some(unfinished));
        assert_eq!(fake.seen().len(), before);
    }

    #[test]
    fn free_names_count_up_from_the_default() {
        let taken = vec!["koidrive".to_owned(), "koidrive-2".to_owned()];
        assert_eq!(free_name("koidrive", &[]), "koidrive");
        assert_eq!(free_name("koidrive", &taken), "koidrive-3");
    }

    #[test]
    fn the_embedded_bundle_reads_back_when_the_build_has_one() {
        match option_env!("KOIDRIVE_SERVER_BUNDLE") {
            None => assert!(
                embedded_bundle()
                    .unwrap_err()
                    .message
                    .contains("built without a server bundle")
            ),
            Some(_) => {
                let bundle = embedded_bundle().unwrap();
                assert!(!bundle.module.is_empty() && !bundle.assets.is_empty());
            }
        }
    }

    #[test]
    fn newer_compares_dotted_numbers_only() {
        assert!(newer("0.10.0", "0.9.9"));
        assert!(!newer("0.3.0", "0.3.0"));
        assert!(!newer("0.2.9", "0.3.0"));
        assert!(newer("0.4.0-rc1", "0.3.0"));
    }

    #[test]
    fn newer_obeys_semver_prerelease_precedence() {
        for (server, cli) in [
            ("0.1.0-alpha.2", "0.1.0-alpha.1"),
            ("0.2.0-alpha.1", "0.1.0-alpha.1"),
            ("0.1.0", "0.1.0-alpha.1"),
            ("0.1.0-alpha.10", "0.1.0-alpha.2"),
            ("0.1.0-rc.1", "0.1.0-alpha.10"),
        ] {
            assert!(newer(server, cli), "{server} must be newer than {cli}");
            assert!(!newer(cli, server), "{cli} must not be newer than {server}");
        }
        assert!(!newer("0.1.0+build.2", "0.1.0+build.1"));
        assert!(!newer("not-a-version", "0.1.0"));
        assert!(!newer("0.1.0", "not-a-version"));
    }
}
