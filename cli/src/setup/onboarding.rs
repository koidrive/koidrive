// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};
use std::sync::Arc;

use koidrive_driver::{
    CreatedSpace, Credential, DirectoryIdentity, DriverError, LinkApproval, Paths, api_for,
};
use koidrive_ipc::{Code, Next, Reason, Request, Response};
use koidrive_net::ApiError;
use koidrive_proto::v1::SpaceInfo;
use koidrive_telemetry::schema::{SetupFlow, SetupStep};
use url::Url;

use super::bundle::bundled;
use super::session::with_session;
use super::state::StateFile;
use super::{DeployArgs, Mode, ScriptArgs, this_mode};
use crate::error::CliError;
use crate::ipc;
use crate::pairing::Invitation;
use crate::printable;
use crate::report::{AccountData, Choice, ChoicesData, Done, FolderData, Outcome, SetupData};
use crate::terminal::{Progress, Terminal, read_line};

const DEFAULT_FOLDER: &str = "Koidrive";

/// The ways to set up this machine, which `koi setup` lists when it does not guide.
pub(super) fn choices() -> Vec<Choice> {
    let choice = |argv: &[&str], description| Choice {
        argv: Next::of(argv).argv,
        description,
    };
    vec![
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
            "Claim the server you deployed, with the claim code its /setup page writes to the Worker's log",
        ),
        choice(
            &["koi", "join", "<CODE>"],
            "Join a server with a pairing code from a device that is already signed in",
        ),
        choice(
            &["koi", "setup", "--interactive"],
            "Choose in the guided setup, on a terminal",
        ),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    Cloudflare,
    Deployed,
    Pairing,
}

impl Route {
    pub(super) const ALL: [Route; 3] = [Route::Cloudflare, Route::Deployed, Route::Pairing];

    fn label(self) -> &'static str {
        match self {
            Route::Cloudflare => "Log in with Cloudflare",
            Route::Deployed => "I deployed a server",
            Route::Pairing => "Enter a pairing code",
        }
    }
}

pub async fn setup(
    paths: &Paths,
    device_name: &str,
    deploy: &DeployArgs,
    script: &ScriptArgs,
    json: bool,
    verbose: bool,
    terminal: Arc<Terminal>,
) -> Result<Done, CliError> {
    if script.given() && deploy.subdomain.is_some() {
        return Err(CliError::usage("--subdomain cannot be used with --folder"));
    }
    if let (Some(folder), Some(space)) = (&script.folder, &script.space) {
        return scripted(paths, device_name, script, folder, space).await;
    }
    let login = &deploy.login;
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
        let mut choices = choices();
        if let Some(subdomain) = &deploy.subdomain {
            choices[0]
                .argv
                .extend(["--subdomain".to_owned(), subdomain.clone()]);
        }
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
        connect(paths, device_name, deploy, verbose, terminal.clone()).await?;
    }
    onboard(paths, &terminal).await
}

async fn connect(
    paths: &Paths,
    device_name: &str,
    deploy: &DeployArgs,
    verbose: bool,
    terminal: Arc<Terminal>,
) -> Result<(), CliError> {
    let labels = Route::ALL
        .iter()
        .map(|route| route.label().to_owned())
        .collect();
    let route = terminal
        .select("How do you want to connect?", labels)
        .await?;
    match Route::ALL.get(route) {
        Some(Route::Cloudflare) => {
            crate::telemetry::set_flow(SetupFlow::Cloudflare);
            bundled()?;
            with_session(&deploy.login, terminal.clone(), verbose, async |session| {
                session.requested_subdomain = deploy.subdomain.clone();
                session.setup(paths, device_name).await
            })
            .await?;
            terminal.complete("This device is connected");
            Ok(())
        }
        Some(Route::Deployed) => deployed(paths, device_name, &terminal).await,
        Some(Route::Pairing) => {
            crate::telemetry::set_flow(SetupFlow::Pairing);
            terminal.separator();
            terminal.message("On your other device, run koi device invite.");
            let code = terminal.input("Pairing code", true).await?;
            join(paths, &code, None, device_name, &terminal).await
        }
        None => Err(CliError::from(
            "the terminal returned an invalid setup choice",
        )),
    }
}

async fn join(
    paths: &Paths,
    code: &str,
    server: Option<&str>,
    device_name: &str,
    terminal: &Terminal,
) -> Result<(), CliError> {
    let progress = terminal.progress("Connecting this device...");
    let joining = async {
        let invitation = invitation(code, server)?;
        let request = Request::Pair {
            server: invitation.server,
            code: invitation.code,
            name: device_name.to_owned(),
        };
        report_paired(paths, ipc::run(paths, request).await?, terminal)
    };
    crate::telemetry::step(SetupStep::Pair, joining).await?;
    progress.finish("This device is connected");
    Ok(())
}

async fn deployed(paths: &Paths, device_name: &str, terminal: &Terminal) -> Result<(), CliError> {
    crate::telemetry::set_flow(SetupFlow::Claim);
    terminal.separator();
    terminal.message(
        "Enter the address of the server you deployed, such as https://koidrive.alice.workers.dev.",
    );
    terminal.message("To deploy a server or find its claim code, follow https://github.com/koidrive/koidrive/blob/main/docs/deploy.md");
    let (api, claimed) = loop {
        let answer = terminal.input("Server address", false).await?;
        if answer.trim().is_empty() {
            terminal.message("Enter the server's address.");
            continue;
        }
        let address = crate::pairing::server_url(&answer);
        let Ok(api) = api_for(&address) else {
            terminal.message(&format!(
                "{} is not a server address. Enter its https:// address.",
                printable(&address)
            ));
            continue;
        };
        let progress = terminal.progress("Checking the server...");
        let info = api.server_info().await;
        drop(progress);
        match info {
            Ok(info) if !info.server_version.is_empty() => break (api, info.claimed),
            Err(ApiError::Transport(_)) => terminal.message(&format!(
                "Koidrive could not reach {}. Check the address and your connection.",
                printable(&address)
            )),
            _ => terminal.message(&format!(
                "{} does not answer as a Koidrive server. Check the address.",
                printable(&address)
            )),
        }
    };
    let server = api.base().as_str().trim_end_matches('/').to_owned();
    let finish_claim = if claimed && koidrive_driver::claim_pending(paths, api.base()) {
        terminal
            .select(
                "How do you want to sign in?",
                vec![
                    "Finish the previous claim".into(),
                    "Enter a pairing code".into(),
                ],
            )
            .await?
            == 0
    } else {
        false
    };
    if finish_claim {
        terminal.message("This device started to claim this server, and the claim did not finish. Enter the same claim code to finish it.");
    } else if claimed {
        crate::telemetry::set_flow(SetupFlow::Pairing);
        terminal.message("This server already has an owner. On a device that is signed in to it, run koi device invite.");
        terminal.message(&format!(
            "If it is yours and no signed-in device is left, add this device with a recovery key: {}",
            crate::error::RECOVERY_STEPS
        ));
        let code = terminal.input("Pairing code", true).await?;
        return join(paths, &code, Some(&server), device_name, terminal).await;
    } else {
        match api.request_claim_code().await {
            Ok(()) => terminal.message("Your server wrote a one-time claim code to its log."),
            Err(_) => terminal.message(&format!(
                "Open {}/setup in a browser. Your server then writes a one-time claim code to its log.",
                printable(&server)
            )),
        }
        terminal.message("In the Cloudflare dashboard, open Workers & Pages, select your Worker, open Observability, and search for \"Koidrive claim code\".");
    }
    loop {
        let answer = terminal.input("Claim code", true).await?;
        let code = answer.trim();
        if code.is_empty() {
            terminal.message("Enter the claim code.");
            continue;
        }
        let progress = terminal.progress("Claiming your server...");
        let request = Request::Claim {
            server: server.clone(),
            code: code.to_owned(),
            user_name: super::claim::user_name(),
            name: device_name.to_owned(),
        };
        let claiming = async { report_paired(paths, ipc::run(paths, request).await?, terminal) };
        match crate::telemetry::step(SetupStep::Claim, claiming).await {
            Ok(()) => {
                progress.finish("This device is connected as your server's owner");
                return Ok(());
            }
            Err(error) if error.code == Code::WrongCode => {
                drop(progress);
                api.request_claim_code().await.ok();
                terminal.message("The claim code is wrong or has expired. Enter the newest Koidrive claim code from the log.");
            }
            Err(error)
                if claimed
                    && error.reason == Some(Reason::Owned)
                    && koidrive_driver::claim_pending(paths, api.base()) =>
            {
                drop(progress);
                terminal.message("That is not the code this device used. Enter the same claim code to finish the claim.");
            }
            Err(error) => return Err(error),
        }
    }
}

async fn onboard(paths: &Paths, terminal: &Terminal) -> Result<Done, CliError> {
    let credential = Credential::load(&paths.credential())
        .map_err(DriverError::from)?
        .ok_or(DriverError::NotPaired)?;
    let auth = crate::reading(paths, &credential)?;
    let mut progress = Some(terminal.progress("Preparing ~/Koidrive..."));
    let spaces = auth
        .api()
        .list_spaces(&auth)
        .await
        .map_err(CliError::api_retry_safe)?
        .spaces;
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
            Target::Default => {
                match crate::named_space(paths, &auth, &spaces, DEFAULT_FOLDER).await? {
                    crate::Named::Created(space) => break crate::Named::Created(space),
                    crate::Named::Existing(_) => {
                        drop(progress.take());
                        terminal.message(&format!(
                        "A shared folder named {DEFAULT_FOLDER} is available. Choose which folder to sync."
                    ));
                        let now = auth
                            .api()
                            .list_spaces(&auth)
                            .await
                            .map_err(CliError::api_retry_safe)?;
                        target = shared_folder(terminal, &now.spaces).await?;
                    }
                }
            }
            Target::New(name) => {
                let taken = match koidrive_driver::create_space(paths, &auth, &name).await {
                    Ok(CreatedSpace::Made(space)) => break crate::Named::Created(space),
                    Ok(CreatedSpace::Joined(_)) => None,
                    Err(DriverError::Api(error)) if error.status() == Some(409) => Some(error),
                    Err(error) => return Err(error.into()),
                };
                let now = auth
                    .api()
                    .list_spaces(&auth)
                    .await
                    .map_err(CliError::api_retry_safe)?;
                if let Some(error) = taken
                    && !now.spaces.iter().any(|space| space.name == name)
                {
                    return Err(error.into());
                }
                drop(progress.take());
                terminal.message(&format!(
                    "A shared folder named {} already exists. Choose another name.",
                    printable(&name)
                ));
                target = new_shared_folder(terminal, &now.spaces).await?;
            }
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
        "Setup complete. Add files to {}.\nCheck sync: koi status\nAdd another device: run koi device invite here, then run the command it prints on the other device.\n",
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
    let credential = Credential::load(&paths.credential()).map_err(DriverError::from)?;
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
    invitation(&line, None)
}

fn invitation(code: &str, server: Option<&str>) -> Result<Invitation, CliError> {
    crate::pairing::parse(code, server)
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

/// Refuses a machine that is already set up, unless a run that stopped halfway left its state file: that run checks
/// the saved credential before it claims (section 4.6).
pub(super) fn not_set_up(paths: &Paths) -> Result<(), CliError> {
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

pub(super) fn report_paired(
    paths: &Paths,
    response: Response,
    terminal: &Terminal,
) -> Result<(), CliError> {
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
        other => Err(crate::unexpected(other)),
    }
}
