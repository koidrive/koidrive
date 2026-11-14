// SPDX-License-Identifier: Apache-2.0

//! The `koi` command: shows status and pending decisions, sets up a server or joins one, links folders, lists a
//! folder's trash and a file's versions, manages the account's devices and spaces, and runs the sync daemon. Commands
//! run in the daemon through its socket when it is running, and on their own otherwise. The device, space, trash and
//! versions commands talk to the server directly with the credential. With `--json` every command prints one
//! envelope (see [`report`]) and exits with the status of its [`koidrive_ipc::Code`].

mod app;
mod app_install;
mod diagnose;
mod error;
mod history;
mod ipc;
mod logfile;
mod manifest;
mod notify;
mod pairing;
mod report;
mod request;
#[cfg(test)]
mod schema;
mod service;
mod setup;
mod share;
mod telemetry;
mod terminal;
mod update;

use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::error::ErrorKind;
use clap::{ArgAction, CommandFactory, Parser, Subcommand, ValueEnum};
use koidrive_driver::{
    Answer, Credential, Driver, DriverError, FolderState, FolderStatus, Limit, MAX_DELETIONS,
    Paths, Progress, Question, QuestionKind, RecoveryGroup, Status,
};
use koidrive_ipc::{Code, Key, Request, RequestId, Response};
use koidrive_net::{ApiError, Auth, Reload};
use koidrive_proto::v1::{CreateSpaceRequest, DeviceInfo, SpaceInfo, SpaceUsage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use koidrive_telemetry::schema::{DaemonStart, Failure, SetupStep};

use crate::error::CliError;
use crate::report::{
    AccountData, AnsweredData, Decision, DeviceData, DevicesData, Done, Empty, Envelope,
    FolderData, HelpData, InvitationData, LeftData, Lifecycle, NotRemovedData, RefusedSpace,
    RevokedData, SpaceData, SpacesData, StatusData, UnlinkedData, UpdateNoticeData, UsageData,
    UsagesData, VersionInfoData, WithService,
};
use crate::request::Begun;

#[derive(Parser)]
#[command(
    name = "koi",
    version,
    color = terminal::clap_color(),
    about = "Koidrive sync client.",
    long_about = "Koidrive keeps your folders in sync through your own server.",
    after_help = "Commands ask questions only when a terminal is attached to stdin and stderr.\nAgents: start with koi status --json."
)]
struct Cli {
    /// Print one JSON object with the result. Only schema 1 exists.
    #[arg(
        long,
        global = true,
        value_name = "VERSION",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "1",
        value_parser = schema_version
    )]
    json: Option<u32>,
    #[arg(
        long,
        global = true,
        help = "Use line-oriented output without color or animation"
    )]
    plain: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show sync state, pending decisions, and the commands to resolve them.
    Status {
        /// Exit with 4 when this device is not set up, 5 when a folder is halted or stopped, 3 when a decision needs a
        /// choice, or 6 when a folder is offline, in that order.
        #[arg(long)]
        check: bool,
    },
    /// List pending decisions, or answer one by its ID.
    Resolve {
        /// The decision to make, as koi resolve lists it.
        #[arg(requires = "answer")]
        id: Option<String>,
        /// One of the answers the decision lists, such as restore or keep-node:<node id>.
        answer: Option<String>,
    },
    /// Show the setup choices, or run the guided setup on a terminal.
    Setup {
        #[command(flatten)]
        device: DeviceArgs,
        #[command(flatten)]
        login: setup::LoginArgs,
        #[command(flatten)]
        script: setup::ScriptArgs,
        /// Skip installing and opening the macOS app.
        #[arg(long)]
        no_app: bool,
    },
    /// Add this device with a pairing code from a device that is already paired.
    #[command(group(clap::ArgGroup::new("code_source").required(true).args(["code", "code_stdin"])))]
    Join {
        /// The pairing code, such as K7QM-2XPD@koidrive.alice.workers.dev.
        code: Option<String>,
        /// Read the pairing code from one line of stdin rather than an argument.
        #[arg(long)]
        code_stdin: bool,
        /// The server's address, for a code without @host or to replace its host.
        #[arg(long)]
        server: Option<String>,
        #[command(flatten)]
        device: DeviceArgs,
    },
    /// Deploy, claim, add this device to, upgrade or delete the server in your Cloudflare account.
    #[command(arg_required_else_help = true)]
    Server {
        #[command(subcommand)]
        action: setup::ServerAction,
    },
    /// Create and list spaces, or show their storage usage.
    #[command(arg_required_else_help = true)]
    Space {
        #[command(subcommand)]
        action: SpaceAction,
    },
    /// Link, unlink, restart, or retry a local folder.
    #[command(arg_required_else_help = true)]
    Folder {
        #[command(subcommand)]
        action: FolderAction,
    },
    /// Start the background service that syncs the linked folders.
    #[command(arg_required_else_help = true)]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Pause sync on this device until koi resume, also across restarts. The folders stay linked.
    Pause,
    /// Resume sync after koi pause.
    Resume,
    /// Invite, list, or revoke devices.
    #[command(arg_required_else_help = true)]
    Device {
        #[command(subcommand)]
        action: DeviceAction,
    },
    /// List trash, versions, and restore requests, or request a restore.
    #[command(arg_required_else_help = true)]
    History {
        #[command(subcommand)]
        action: history::HistoryAction,
    },
    /// Make, list, or revoke read-only links to files and folders that anyone with the link can open.
    #[command(arg_required_else_help = true)]
    Share {
        #[command(subcommand)]
        action: share::ShareAction,
    },
    /// Show, turn on or off, preview or reset the anonymous usage reports this machine sends.
    #[command(arg_required_else_help = true)]
    Telemetry {
        #[command(subcommand)]
        action: telemetry::TelemetryAction,
    },
    /// Upgrade Koidrive to the newest release.
    Update,
    /// Write a file for a report of a problem: the version, the system, the status and the daemon's log, with your
    /// paths, file names, space and device names, server address and tokens replaced.
    Diagnose,
    /// Run the sync daemon in the foreground until SIGTERM or SIGINT.
    #[command(name = "_daemon", hide = true)]
    Daemon {
        #[arg(long, value_enum, default_value_t = LogFormat::Text)]
        log_format: LogFormat,
    },
    /// Bring the background service in line with the linked folders, as linking a folder does. It leaves koi switched
    /// off in Login Items off and never resumes a pause.
    #[command(name = "_reconcile", hide = true)]
    Reconcile {
        #[arg(long, hide = true)]
        no_app: bool,
    },
}

impl Command {
    /// Whether the command reports the decisions pending in the profile after its own output: on stderr as a banner,
    /// or in the JSON envelope. `status` and a `resolve` that lists show the decisions themselves, and the daemon never
    /// ends. A `resolve` that answers reports the decisions still pending.
    fn reports_pending(&self) -> bool {
        !matches!(
            self,
            Command::Daemon { .. } | Command::Status { .. } | Command::Resolve { id: None, .. }
        )
    }
}

impl Cli {
    /// Whether the command ends with the pending-decision banner on stderr. JSON output carries decisions as data,
    /// without decoration, and `resolve` names itself.
    fn shows_banner(&self) -> bool {
        self.json.is_none()
            && (self.command.as_ref()).is_some_and(|command| {
                command.reports_pending() && !matches!(command, Command::Resolve { .. })
            })
    }
}

#[derive(Subcommand)]
enum SpaceAction {
    /// Create a space. Link a folder to it with koi folder link.
    Create {
        /// The space's name, at most 255 bytes.
        #[arg(value_parser = short_name)]
        name: String,
    },
    /// List the spaces of the server.
    List,
    /// Show how many bytes each space stores: in live files, in the trash and in old versions.
    Usage,
}

#[derive(Subcommand)]
enum FolderAction {
    /// Link a folder to a space and start syncing it.
    Link {
        /// The folder to sync. It must exist and may already hold files.
        folder: PathBuf,
        /// The space's id or name.
        #[arg(long)]
        space: String,
    },
    /// Stop syncing a folder. Its files stay where they are.
    Unlink {
        /// The linked folder.
        folder: PathBuf,
    },
    /// Sync a linked folder again from the start, after fixing the problem that halted it.
    Restart {
        /// The linked folder.
        folder: PathBuf,
        #[command(flatten)]
        request: RequestArg,
    },
    /// Sync a folder again whose changes the server refused, after fixing the cause the status names.
    #[command(group = clap::ArgGroup::new("how").required(true).args(["resend", "replan"]))]
    Retry {
        /// The linked folder whose changes the server refused.
        folder: PathBuf,
        /// Send the refused changes again as they were, for a cause fixed on the server.
        #[arg(long)]
        resend: bool,
        /// Plan the refused changes again into a new commit, for a cause fixed on this device.
        #[arg(long)]
        replan: bool,
        #[command(flatten)]
        request: RequestArg,
    },
}

/// `--request-id`, for a change an agent may run again after a timeout.
#[derive(clap::Args)]
pub struct RequestArg {
    /// An ID you pick for this change, of letters, digits, - and _. Running the command again with the same ID and
    /// arguments returns the first outcome and changes nothing again.
    #[arg(long, value_name = "ID", value_parser = RequestId::parse)]
    pub request_id: Option<RequestId>,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install and start the service, after fixing what kept it from running, such as koi switched off in Login Items.
    Start,
}

#[derive(Subcommand)]
enum DeviceAction {
    /// Show a pairing code that adds another device to this server. It works once, for 10 minutes.
    Invite {
        #[command(flatten)]
        request: RequestArg,
    },
    /// List the devices paired with the server, revoked ones included.
    List,
    /// Revoke another device, such as a lost one. It stops syncing at once, and the files on it stay there.
    Revoke {
        /// The device's ID, as koi device list shows it.
        #[arg(value_parser = device_id)]
        id: String,
    },
    /// Sign this device out: revoke it on the server and unlink every folder. The files stay where they are.
    Leave {
        /// Sign out even if this is the account's last active device or the server does not answer.
        #[arg(long)]
        last_device: bool,
    },
}

#[derive(clap::Args)]
pub struct DeviceArgs {
    /// A name for this device, shown on the others, at most 255 bytes. Defaults to this machine's host name.
    #[arg(long, value_parser = short_name)]
    name: Option<String>,
    /// Turn anonymous usage reports on or off for this machine, without being asked (see koi telemetry).
    #[arg(long, value_enum)]
    telemetry: Option<telemetry::Switch>,
}

impl DeviceArgs {
    pub fn name(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            let host = rustix::system::uname()
                .nodename()
                .to_string_lossy()
                .into_owned();
            host.split('.').next().unwrap_or_default().to_owned()
        })
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

fn schema_version(text: &str) -> Result<u32, String> {
    match text {
        "1" => Ok(1),
        _ => Err("only JSON schema 1 exists".to_owned()),
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    if let Some(exit) = terminal::helper(&args) {
        return exit;
    }
    let command = command_path(&args);
    if let Some(error) = json_versions(&args).find_map(Result::err) {
        return finish(&command, true, Err(CliError::usage(error)), Vec::new());
    }
    if let Some(names) = help_names(&args).filter(|_| json_requested(&args)) {
        let path = names.join(" ");
        let outcome = help_of(&path)
            .map(|help| help_done(help, &path))
            .ok_or_else(|| CliError::usage(format!("no command is named {}", printable(&path))));
        return finish(format!("help {path}").trim_end(), true, outcome, Vec::new());
    }
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => return parse_failure(error, &args, &command, json_requested(&args)),
    };
    let json = cli.json.is_some();
    let shows_banner = cli.shows_banner();
    let Some(command_args) = cli.command else {
        let help = Cli::command().render_help().to_string();
        return finish(&command, json, Ok(help_done(help, "")), Vec::new());
    };
    let reports_pending = command_args.reports_pending();
    let interactive = match &command_args {
        Command::Setup { login, .. } => login.interactive,
        Command::Server { action } => action.login().is_some_and(|login| login.interactive),
        _ => false,
    };
    let scripted = matches!(&command_args, Command::Setup { script, .. } if script.given());
    let guided = !scripted
        && setup::mode(
            io::stdin().is_terminal(),
            io::stderr().is_terminal(),
            json,
            std::env::var_os("CI").is_some(),
            interactive,
        )
        .is_ok_and(|mode| mode == setup::Mode::Guided);
    let terminal = Arc::new(
        if matches!(command_args, Command::Setup { .. }) && !scripted {
            terminal::Terminal::setup(cli.plain, guided)
        } else {
            terminal::Terminal::new(cli.plain, guided)
        },
    );
    let log_format = match &command_args {
        Command::Daemon { .. } if json => Some(LogFormat::Json),
        Command::Daemon { log_format } => Some(*log_format),
        _ => None,
    };
    let paths = match Paths::from_env() {
        Ok(paths) => paths,
        Err(error) => return finish(&command, json, Err(error.to_string().into()), Vec::new()),
    };
    let agent_log = (log_format.is_some())
        .then(|| service::this_service(&paths).ok())
        .flatten()
        .and_then(|service| logfile::CappedLog::of_stderr(service.log_file()));
    init_logging(
        log_format,
        json || !console::colors_enabled_stderr(),
        agent_log,
    );
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let error = CliError::from(format!("cannot start: {error}"));
            return finish(&command, json, Err(error), Vec::new());
        }
    };
    let daemon_run = matches!(command_args, Command::Daemon { .. });
    let pending_paths = paths.clone();
    let telemetry_paths = paths.clone();
    let (name, flow, switch) = telemetry_of(&command_args);
    let ask = guided
        && !daemon_run
        && (matches!(command_args, Command::Setup { .. }) || telemetry::prompt_pending(&paths));
    let command_name = command.as_str();
    let work_terminal = terminal.clone();
    runtime.block_on(async {
        let sigint = (!daemon_run).then(|| signal(SignalKind::interrupt()));
        let sigterm = (!daemon_run).then(|| signal(SignalKind::terminate()));
        let interrupt = async move {
            let (Some(Ok(mut sigint)), Some(Ok(mut sigterm))) = (sigint, sigterm) else {
                return std::future::pending().await;
            };
            loop {
                let received = tokio::select! {
                    received = sigint.recv() => received.map(|()| 130),
                    received = sigterm.recv() => received.map(|()| 143),
                };
                let Some(status) = received else {
                    return std::future::pending().await;
                };
                if !setup::in_session() {
                    return Ok::<u8, ()>(status);
                }
            }
        };
        tokio::select! {
            done = async move {
                telemetry::start(&paths, switch, name, flow).await;
                let mut outcome = run(paths, command_args, json, work_terminal.clone()).await;
                telemetry::finish().await;
                let pending = match reports_pending && (json || shows_banner) {
                    true => pending_questions(&pending_paths).await,
                    false => Vec::new(),
                };
                let succeeded = outcome.is_ok();
                let prompt_eligible = outcome.as_ref().is_ok_and(|done| done.exit == 0);
                if json && daemon_run && succeeded {
                    return ExitCode::SUCCESS;
                }
                let summary = if work_terminal.setup_guided() {
                    outcome.as_mut().ok().map(|done| std::mem::take(&mut done.note))
                } else {
                    None
                };
                let exit = finish(command_name, json, outcome, pending);
                if ask && prompt_eligible
                    && let Err(error) = telemetry::ask(&telemetry_paths, &work_terminal).await
                {
                    return finish(command_name, json, Err(error), Vec::new());
                }
                if let Some(summary) = summary {
                    work_terminal.outro(&summary);
                }
                exit
            } => done,
            Ok(status) = interrupt => {
                terminal.cancel().await;
                app::cancel_active().await;
                let error = CliError::new(
                    Code::Interrupted,
                    if status == 143 { "stopped by SIGTERM; what the command already did stays done" } else { "interrupted by Ctrl-C; what the command already did stays done" },
                );
                telemetry::flush();
                let _ = finish(&command, json, Err(error), Vec::new());
                let _ = io::Write::flush(&mut io::stdout());
                std::process::exit(status.into());
            }
        }
    })
}

/// How a command shows in usage reports: its name, the setup flow it runs when that is known before it starts, and
/// its `--telemetry` flag. `koi setup` learns its flow from the person's choice.
fn telemetry_of(
    command: &Command,
) -> (
    koidrive_telemetry::schema::Command,
    Option<koidrive_telemetry::schema::SetupFlow>,
    Option<telemetry::Switch>,
) {
    use koidrive_telemetry::schema::{Command as Name, SetupFlow as Flow};
    use setup::ServerAction;
    match command {
        Command::Status { .. } => (Name::Status, None, None),
        Command::Resolve { .. } => (Name::Resolve, None, None),
        Command::Setup { device, .. } => (Name::Setup, None, device.telemetry),
        Command::Join { device, .. } => (Name::Join, Some(Flow::Join), device.telemetry),
        Command::Server { action } => {
            let (flow, device) = match action {
                ServerAction::Deploy { device, .. } => (Flow::Deploy, Some(device)),
                ServerAction::AddDevice { device, .. } => (Flow::AddDevice, Some(device)),
                ServerAction::Claim { device, .. } => (Flow::Claim, Some(device)),
                ServerAction::Upgrade { .. } => (Flow::Upgrade, None),
                ServerAction::Delete { .. } => (Flow::Uninstall, None),
            };
            (
                Name::Server,
                Some(flow),
                device.and_then(|device| device.telemetry),
            )
        }
        Command::Space { .. } => (Name::Space, None, None),
        Command::Folder { .. } => (Name::Folder, None, None),
        Command::Device { .. } => (Name::Device, None, None),
        Command::History { .. } => (Name::History, None, None),
        Command::Service { .. } | Command::Reconcile { .. } => (Name::Service, None, None),
        Command::Pause => (Name::Pause, None, None),
        Command::Resume => (Name::Resume, None, None),
        Command::Share { .. } => (Name::Share, None, None),
        Command::Telemetry { .. } => (Name::Telemetry, None, None),
        Command::Diagnose => (Name::Diagnose, None, None),
        Command::Update => (Name::Update, None, None),
        Command::Daemon { .. } => (Name::Daemon, None, None),
    }
}

/// Runs the command. After one that may change the folders this profile links or its pairing, `service start` or
/// `_reconcile`, it brings the background service in line with the profile and reports it as `service`.
async fn run(
    paths: Paths,
    command: Command,
    json: bool,
    terminal: Arc<terminal::Terminal>,
) -> Result<Done, CliError> {
    let lifecycle_deadline = matches!(command, Command::Reconcile { .. })
        .then(|| tokio::time::Instant::now() + std::time::Duration::from_secs(380));
    let app_policy = match &command {
        Command::Setup { no_app, .. } => Some(if *no_app {
            Err("opt_out")
        } else if !terminal.guided() {
            Err("unattended_setup")
        } else {
            Ok(app_install::AppLaunch::Open)
        }),
        Command::Reconcile { no_app } => Some(if *no_app {
            Err("service_handoff")
        } else if std::env::current_exe().is_ok_and(|binary| app::bundled(&binary)) {
            Err("bundled_helper")
        } else {
            Ok(app_install::AppLaunch::Preserve)
        }),
        _ => None,
    };
    let setup_app = matches!(command, Command::Setup { .. });
    let asked = matches!(command, Command::Service { .. });
    let shows_state = asked || matches!(command, Command::Reconcile { .. });
    let reconciles = shows_state
        || matches!(
            command,
            Command::Folder {
                action: FolderAction::Link { .. } | FolderAction::Unlink { .. }
            } | Command::Device {
                action: DeviceAction::Leave { .. }
            } | Command::Resume
                | Command::Setup { .. }
                | Command::Join { .. }
                | Command::Server {
                    action: setup::ServerAction::Claim { .. }
                        | setup::ServerAction::Deploy { .. }
                        | setup::ServerAction::AddDevice { .. }
                }
        );
    let done = execute(paths.clone(), command, json, terminal.clone()).await?;
    let service = if reconciles {
        let _progress = terminal
            .setup_guided()
            .then(|| terminal.progress("Starting background sync..."));
        let report = service::reconcile(&paths, asked).await;
        let failed = shows_state
            && report.failed()
            && (asked || report.state != service::State::SwitchedOff || report.problem.is_some());
        let failure = failed.then(|| report.failure_message());
        if let Some(message) = failure {
            return Err(CliError {
                partial: Some(Box::new(with_service(done, report))),
                ..CliError::new(Code::Failed, message)
            });
        }
        Some(report)
    } else {
        None
    };
    let app = match app_policy {
        Some(policy) => Some(match policy {
            Ok(launch) => {
                let _progress = (terminal.setup_guided() && cfg!(target_os = "macos"))
                    .then(|| terminal.progress("Preparing the Koidrive app..."));
                let deadline = lifecycle_deadline.unwrap_or_else(|| {
                    tokio::time::Instant::now() + std::time::Duration::from_secs(400)
                });
                app::install(&paths, &service::this_version(), launch, deadline).await
            }
            Err(reason) => (app::Report::skipped(reason), None),
        }),
        None => None,
    };
    if terminal.setup_guided() {
        finish_setup(&paths, done, service, app, &terminal).await
    } else {
        finish_lifecycle(done, service, app, setup_app, shows_state)
    }
}

async fn finish_setup(
    paths: &Paths,
    done: Done,
    service: Option<service::Report>,
    app: Option<(app::Report, Option<service::Report>)>,
    terminal: &terminal::Terminal,
) -> Result<Done, CliError> {
    let final_service = app
        .as_ref()
        .and_then(|(_, report)| report.as_ref())
        .or(service.as_ref());
    let progress = terminal.continue_progress("Starting background sync...");
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ipc::run(paths, Request::Status),
    )
    .await;
    drop(progress);
    let status = match status {
        Ok(Ok(Response::Status { status })) => Some(status),
        Ok(Ok(_)) => None,
        Ok(Err(error)) => {
            terminal.message(&error.message);
            None
        }
        Err(_) => None,
    };
    let mut summary = done.note.clone();
    match final_service {
        Some(report) if report.state == service::State::Off => {
            terminal.message("Turn on background sync in config.toml, then run koi service start.");
            summary =
                "Folder linked. Background sync is off in config.toml.\nStart sync: koi service start\n".into();
        }
        Some(report)
            if report.failed()
                || !report.blocked.is_empty()
                || report.state != service::State::Running =>
        {
            if report.state == service::State::Running {
                if let Some(problem) = &report.problem {
                    terminal.message(problem);
                }
                for blocked in &report.blocked {
                    terminal.message(&format!(
                        "macOS can't read {} in the background.",
                        setup::shown_folder(Path::new(&blocked.folder))
                    ));
                    if blocked.waiting {
                        terminal.message("If macOS asks for folder access, click Allow.");
                    } else {
                        terminal.message("Allow folder access in System Settings > Privacy & Security > Files & Folders, or add koi under Full Disk Access.");
                        terminal.message(&format!(
                            "Then run: koi folder restart {}",
                            pairing::shell_word(&blocked.folder)
                        ));
                    }
                }
            } else {
                let reason = report.text();
                for line in reason.lines() {
                    terminal.message(line);
                }
            }
            summary =
                "Folder linked. Background sync needs attention.\nCheck sync: koi status\n".into();
        }
        _ => {
            match status {
                Some(status) if status.paused => {
                    terminal.message("Sync is paused.");
                    summary = "Folder linked. Sync is paused.\nResume sync: koi resume\n".into();
                }
                Some(status)
                    if final_service.is_some() && status.running && !status.folders.is_empty() =>
                {
                    let blocked = status.folders.iter().find(|folder| {
                        folder.questions > 0
                            || matches!(
                                folder.state,
                                FolderState::Stopped | FolderState::Halted | FolderState::Offline
                            )
                    });
                    if let Some(folder) = blocked {
                        let state = match folder.state {
                            FolderState::Stopped => Some("stopped"),
                            FolderState::Halted => Some("halted"),
                            FolderState::Offline => Some("offline"),
                            _ => None,
                        };
                        if let Some(state) = state {
                            terminal.message(&format!(
                                "Sync is {state} for {}.",
                                setup::shown_folder(Path::new(&folder.path))
                            ));
                        }
                        if folder.questions > 0 {
                            terminal.message(&format!(
                                "Review pending decisions for {}: koi resolve",
                                setup::shown_folder(Path::new(&folder.path))
                            ));
                        }
                        if let Some(problem) = &folder.problem {
                            terminal.message(problem);
                        }
                        summary =
                            "Folder linked. Sync needs attention.\nCheck sync: koi status\n".into();
                    } else {
                        terminal.complete("Background sync is running");
                    }
                }
                _ => {
                    summary = "Folder linked. Could not confirm background sync.\nCheck sync: koi status\n".into();
                }
            }
        }
    }
    if let Some(report) = final_service
        && report.manager == service::ServiceManager::Systemd
        && report.state == service::State::Running
        && report.lingers == Some(false)
    {
        terminal.message("Background sync stops when you log out. To keep it running, run: sudo loginctl enable-linger \"$USER\"");
    }
    if let Some((report, _)) = &app {
        match report {
            app::Report::Present { .. } if !report.failed() => {
                terminal.complete("Koidrive app is ready")
            }
            _ if report.failed() => {
                for line in report.text().lines() {
                    terminal.message(line);
                }
            }
            _ => {}
        }
    }
    let mut result = finish_lifecycle(done, service, app, true, false);
    match &mut result {
        Ok(done) => {
            done.text.clear();
            done.note = summary;
        }
        Err(error) => {
            if let Some(done) = &mut error.partial {
                done.text.clear();
                done.note.clear();
            }
        }
    }
    result
}

fn with_service(mut done: Done, report: service::Report) -> Done {
    let data = match done.data.take() {
        Value::Null => Value::Object(serde_json::Map::new()),
        data => data,
    };
    done.data = serde_json::to_value(WithService {
        data,
        service: report,
    })
    .expect("the CLI's own types serialize");
    done
}

fn finish_lifecycle(
    mut done: Done,
    service: Option<service::Report>,
    app: Option<(app::Report, Option<service::Report>)>,
    setup_app: bool,
    shows_state: bool,
) -> Result<Done, CliError> {
    let (app, handoff) = app.map_or((None, None), |(app, service)| (Some(app), service));
    let service = if setup_app {
        handoff.or(service)
    } else {
        service
    };
    if let Some(report) = service {
        let text = match report.text() {
            text if text.is_empty() && shows_state => report.status_line(),
            text => text,
        };
        done = with_service(done, report);
        done.text.push_str(&text);
    }
    if let Some(report) = app {
        let failed = setup_app && report.failed();
        done.text.push_str(&report.text());
        done.data = serde_json::to_value(report::WithApp {
            data: done.data,
            app: Some(report),
        })
        .expect("the CLI's own types serialize");
        if failed {
            return Err(CliError {
                partial: Some(Box::new(done)),
                ..CliError::new(
                    Code::Failed,
                    "The device is connected, but app setup is incomplete; run koi setup to retry, or use --no-app",
                )
            });
        }
    }
    Ok(done)
}

async fn execute(
    paths: Paths,
    command: Command,
    json: bool,
    terminal: Arc<terminal::Terminal>,
) -> Result<Done, CliError> {
    match command {
        Command::Daemon { .. } => daemon(paths, json).await,
        Command::Status { check } => status(&paths, check).await,
        Command::Diagnose => diagnose::run(&paths).await,
        Command::Update => update::run(&paths, json).await,
        Command::Resolve { id, answer } => resolve(&paths, id, answer).await,
        Command::Setup {
            device,
            login,
            script,
            ..
        } => setup::setup(&paths, &device.name(), &login, &script, json, terminal).await,
        Command::Join {
            code,
            code_stdin,
            server,
            device,
        } => {
            let join = async {
                let code = setup::read_code(code, code_stdin).await?;
                let invitation =
                    pairing::parse(&code, server.as_deref()).map_err(CliError::usage)?;
                let request = Request::Pair {
                    server: invitation.server,
                    code: invitation.code,
                    name: device.name(),
                };
                paired(&paths, ipc::run(&paths, request).await?)
            };
            telemetry::step(SetupStep::Pair, join).await
        }
        Command::Server {
            action:
                setup::ServerAction::Claim {
                    server,
                    code,
                    code_stdin,
                    user,
                    device,
                },
        } => {
            let code = setup::read_code(code, code_stdin).await?;
            let request = Request::Claim {
                server: pairing::server_url(&server),
                code: code.trim().to_owned(),
                user_name: user,
                name: device.name(),
            };
            let claim = async { paired(&paths, ipc::run(&paths, request).await?) };
            telemetry::step(SetupStep::Claim, claim).await
        }
        Command::Server { action } => setup::server(&paths, action, json, terminal).await,
        Command::Space { action } => space(paths, action).await,
        Command::Folder { action } => folder(&paths, action).await,
        Command::Service {
            action: ServiceAction::Start,
        }
        | Command::Reconcile { .. } => Ok(Done::ok(Empty {}, "")),
        Command::Pause => pause(&paths).await,
        Command::Resume => resume(&paths).await,
        Command::Device { action } => device(paths, action).await,
        Command::History { action } => history::run(&paths, action).await,
        Command::Share { action } => share::run(&paths, action).await,
        Command::Telemetry { action } => telemetry::command(&paths, action),
    }
}

/// Prints what the command produced and returns its exit status. Without `--json` the text goes to stdout and an error
/// to stderr; with it, one envelope goes to stdout. Pending decisions follow as a banner on stderr, or as the
/// envelope's decisions.
fn finish(
    command: &str,
    json: bool,
    outcome: Result<Done, CliError>,
    pending: Vec<Question>,
) -> ExitCode {
    let (exit, envelope, text, stderr) = match outcome {
        Ok(done) => {
            let exit = done.exit;
            let (text, note) = (done.text.clone(), done.note.clone());
            (exit, Envelope::of(command, done), text, note)
        }
        Err(mut error) => {
            let partial = error.partial.take().map(|partial| *partial);
            let text = (partial.as_ref()).map_or_else(String::new, |done| done.text.clone());
            let message = format!("koi: {}\n", printable(&error.message));
            let exit = error.code.exit();
            (
                exit,
                Envelope::failed(command, error, partial),
                text,
                message,
            )
        }
    };
    if json && command == "_daemon" {
        println!(
            "{}",
            serde_json::to_string(&envelope).expect("the envelope serializes")
        );
        return ExitCode::from(exit);
    }
    if json {
        let mut envelope = envelope;
        if envelope.decisions.is_empty() {
            envelope.decisions = pending.iter().map(Decision::of).collect();
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&envelope).expect("the envelope serializes")
        );
        return ExitCode::from(exit);
    }
    print!("{text}");
    eprint!("{stderr}");
    if let Some(banner) = banner(&pending) {
        eprintln!("{banner}");
    }
    ExitCode::from(exit)
}

/// What clap reports when it parses no command: help and the version are results, anything else a usage error.
/// Without `--json` clap prints them itself, except that a group named without its subcommand shows its help on
/// stdout and succeeds, like `koi` alone.
fn parse_failure(error: clap::Error, args: &[OsString], command: &str, json: bool) -> ExitCode {
    let kind = error.kind();
    let bare_group = matches!(
        kind,
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand | ErrorKind::MissingSubcommand
    );
    let shown = matches!(kind, ErrorKind::DisplayHelp | ErrorKind::DisplayVersion);
    if !json && shown {
        error.exit();
    }
    let rendered = escape_echoed(&error, args);
    if !json && !bare_group {
        eprint!("{rendered}");
        return ExitCode::from(2);
    }
    let outcome = match kind {
        _ if bare_group => Ok(help_done(
            help_of(command).expect("clap parsed the path"),
            command,
        )),
        ErrorKind::DisplayHelp => Ok(help_done(rendered, command)),
        ErrorKind::DisplayVersion => Ok(Done::ok(
            VersionInfoData {
                version: env!("CARGO_PKG_VERSION"),
            },
            rendered,
        )),
        _ => Err(CliError::usage(rendered.trim_end())),
    };
    finish(command, json, outcome, Vec::new())
}

/// Clap's message for arguments it refused, with every argument it echoes escaped so that a control character in one
/// cannot forge a line. Clap echoes parts of an argument, so the message comes from parsing the escaped arguments
/// again, which keeps the message's own line breaks; should those parse, the whole message is escaped instead.
fn escape_echoed(error: &clap::Error, args: &[OsString]) -> String {
    let escaped: Vec<OsString> = (args.iter())
        .map(|arg| match arg.to_str() {
            Some(text) => printable(text).into(),
            None => arg.clone(),
        })
        .collect();
    if escaped == args {
        return error.render().to_string();
    }
    match Cli::try_parse_from(&escaped) {
        Err(again) if again.kind() == error.kind() => again.render().to_string(),
        _ => printable(&error.render().to_string()),
    }
}

/// The help of the command at `path`, such as `folder`, or none when no command has that path.
fn help_of(path: &str) -> Option<String> {
    let mut root = Cli::command();
    root.build();
    let mut command = &mut root;
    for name in path.split_whitespace() {
        command = command.find_subcommand_mut(name)?;
    }
    Some(command.render_help().to_string())
}

/// The command names after `help` when `help` is the first command named, as in `koi help folder --json`.
/// Clap's own `help` reads every argument as a command name, `--json` included.
fn help_names(args: &[OsString]) -> Option<Vec<&str>> {
    let mut words = (args.iter().skip(1)).take_while(|arg| *arg != "--");
    let mut command = Cli::command();
    let mut path = Vec::new();
    while let Some(word) = words.next() {
        let word = word.to_str()?;
        if word == "--json" || word.starts_with("--json=") {
            continue;
        }
        if word == "help" && command.has_subcommands() {
            let names = words.filter_map(|arg| arg.to_str());
            path.extend(names.filter(|arg| !arg.starts_with('-')));
            return Some(path);
        }
        command = command.find_subcommand(word)?.clone();
        path.push(word);
    }
    None
}

fn help_done(help: String, command: &str) -> Done {
    let data = HelpData {
        help: help.clone(),
        commands: manifest::commands(command),
    };
    Done::ok(data, help)
}

/// Whether the arguments ask for JSON, read before clap parses them so that a usage error is JSON too.
/// The schema versions the arguments name with `--json=<version>`, each read as clap reads it.
fn json_versions(args: &[OsString]) -> impl Iterator<Item = Result<u32, String>> + '_ {
    (args.iter().skip(1))
        .take_while(|arg| *arg != "--")
        .filter_map(|arg| arg.to_str()?.strip_prefix("--json="))
        .map(schema_version)
}

fn json_requested(args: &[OsString]) -> bool {
    (args.iter().skip(1))
        .take_while(|arg| *arg != "--")
        .filter_map(|arg| arg.to_str())
        .any(|arg| arg == "--json" || arg.starts_with("--json="))
}

/// The subcommand path the arguments name, such as `folder link`: each argument that names a subcommand of the one before.
fn command_path(args: &[OsString]) -> String {
    let mut command = Cli::command();
    command.build();
    let mut path = Vec::new();
    for arg in args.iter().skip(1).filter_map(|arg| arg.to_str()) {
        if arg == "--" || shows_and_exits(&command, arg) {
            break;
        }
        if arg.starts_with('-') {
            continue;
        }
        let found = command.find_subcommand(arg);
        let Some(sub) = found.filter(|sub| sub.get_name() != "help").cloned() else {
            break;
        };
        path.push(sub.get_name().to_owned());
        command = sub;
    }
    path.join(" ")
}

fn shows_and_exits(command: &clap::Command, arg: &str) -> bool {
    let exits = |found: Option<&clap::Arg>| {
        found.is_some_and(|found| {
            matches!(
                found.get_action(),
                ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
            )
        })
    };
    if let Some(long) = arg.strip_prefix("--") {
        let name = long.split('=').next().unwrap_or(long);
        return exits(
            command
                .get_arguments()
                .find(|found| found.get_long() == Some(name)),
        );
    }
    let Some(shorts) = arg.strip_prefix('-') else {
        return false;
    };
    for short in shorts.chars() {
        let found = command
            .get_arguments()
            .find(|found| found.get_short() == Some(short));
        if exits(found) {
            return true;
        }
        if found.is_none_or(|found| found.get_action().takes_values()) {
            return false;
        }
    }
    false
}

/// Logs go to stderr: structured key-value text by default, one JSON object per line with `--log-format json`. The
/// level comes from `KOIDRIVE_LOG` (`info` for the daemon, `warn` for other commands). No log line carries a
/// credential, an access token or a pairing code.
fn init_logging(daemon: Option<LogFormat>, json: bool, agent_log: Option<logfile::CappedLog>) {
    let default = if daemon.is_some() { "info" } else { "warn" };
    let filter =
        EnvFilter::try_from_env("KOIDRIVE_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let writer = match agent_log {
        Some(log) => BoxMakeWriter::new(log),
        None => BoxMakeWriter::new(std::io::stderr),
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(log_ansi(std::io::stderr().is_terminal(), json))
        .with_writer(writer);
    match daemon {
        Some(LogFormat::Json) => builder.json().with_current_span(false).init(),
        _ => builder.init(),
    }
}

/// Whether log lines carry ANSI colours: only on a terminal, and never under `--json`.
fn log_ansi(stderr_tty: bool, json: bool) -> bool {
    stderr_tty && !json
}

/// The sync daemon, until SIGTERM or SIGINT. With `json` it prints one JSON object per line on stdout as it starts,
/// as a folder changes state, as it stops and once it stopped, and its logs on stderr are JSON lines too.
async fn daemon(paths: Paths, json: bool) -> Result<Done, CliError> {
    let emit = |event: Lifecycle| {
        if json {
            println!("{}", event.line());
        }
    };
    let failed = |why| telemetry::failed(Failure::DaemonStart(why));
    let driver = daemon_driver(paths).await.map_err(|error| match error {
        DriverError::Busy(_) => {
            failed(DaemonStart::Busy);
            CliError::from("another Koidrive daemon or command is using this profile")
        }
        other => {
            failed(DaemonStart::Profile);
            CliError::from(other)
        }
    })?;
    let socket = driver.paths().socket();
    let listener = koidrive_ipc::bind(&socket).map_err(|error| {
        failed(DaemonStart::Socket);
        format!("cannot listen on {}: {error}", socket.display())
    })?;
    let signals = signal(SignalKind::terminate()).and_then(|terminate| {
        signal(SignalKind::interrupt()).map(|interrupt| (terminate, interrupt))
    });
    let (mut terminate, mut interrupt) = signals.map_err(|error| {
        failed(DaemonStart::Signals);
        error.to_string()
    })?;
    let notifications = notify::decisions(driver.clone());
    driver.start();
    driver.check_for_updates();
    let newest = tokio::spawn(update::follow_newest(driver.paths().clone()));
    let server = tokio::spawn(ipc::serve(listener, driver.clone()));
    let notifications = tokio::spawn(notifications);
    let (stop_telemetry, telemetry_stopped) = tokio::sync::oneshot::channel();
    let telemetry = tokio::spawn(telemetry::run_daemon(telemetry_stopped, driver.clone()));
    let watch = driver.watch();
    let folders = driver.status().folders;
    tracing::info!(
        socket = %socket.display(),
        version = env!("CARGO_PKG_VERSION"),
        folders = folders.len(),
        "the daemon is running"
    );
    emit(Lifecycle::Started {
        version: env!("CARGO_PKG_VERSION"),
        socket: socket.display().to_string(),
        folders: folders.len(),
    });
    let states = json.then(|| tokio::spawn(folder_states(driver.clone(), watch, folders)));
    let (signal, name) = tokio::select! {
        _ = terminate.recv() => (report::Signal::Term, "SIGTERM"),
        _ = interrupt.recv() => (report::Signal::Int, "SIGINT"),
    };
    tracing::info!(signal = name, "stopping the daemon");
    if let Some(states) = states {
        states.abort();
        let _ = states.await;
    }
    emit(Lifecycle::Stopping { signal });
    server.abort();
    notifications.abort();
    newest.abort();
    let _ = std::fs::remove_file(&socket);
    driver.stop().await;
    let _ = stop_telemetry.send(());
    let _ = telemetry.await;
    tracing::info!("the daemon stopped");
    emit(Lifecycle::Stopped);
    Ok(Done::printed())
}

async fn daemon_driver(paths: Paths) -> Result<Driver, DriverError> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match Driver::open_with_telemetry(paths.clone(), telemetry::recorder()) {
            Err(DriverError::Busy(_))
                if tokio::time::Instant::now() < deadline
                    && tokio::net::UnixStream::connect(paths.socket())
                        .await
                        .is_err() =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            opened => return opened,
        }
    }
}

/// Prints a `folder_state` line whenever a folder's state differs from the one last printed for it. `watch` was
/// subscribed before `folders` was read, so a change in between still wakes the loop.
async fn folder_states(
    driver: Driver,
    mut watch: tokio::sync::watch::Receiver<()>,
    folders: Vec<FolderStatus>,
) {
    let mut shown: std::collections::HashMap<String, FolderState> = (folders.into_iter())
        .map(|folder| (folder.path, folder.state))
        .collect();
    while watch.changed().await.is_ok() {
        for folder in driver.status().folders {
            if shown.get(&folder.path) == Some(&folder.state) {
                continue;
            }
            shown.insert(folder.path.clone(), folder.state);
            let data = FolderData::of(&folder);
            let event = Lifecycle::FolderState {
                folder: data.path,
                space_id: data.space_id,
                state: data.state,
            };
            println!("{}", event.line());
        }
    }
}

/// An answer of the daemon that does not fit the request.
fn unexpected(response: Response) -> CliError {
    CliError::from(format!("the daemon answered {response:?}"))
}

fn paired(paths: &Paths, response: Response) -> Result<Done, CliError> {
    let Response::Paired { account } = response else {
        return Err(unexpected(response));
    };
    let text = format!(
        "Paired as {} ({}) with {}.\nThe credential is in {}.\n",
        printable(&account.device_name),
        printable(&account.device_id),
        printable(&account.server),
        printable(&paths.credential().display().to_string())
    );
    Ok(Done::ok(AccountData::of(&account), text))
}

/// The decisions waiting in the profile.
async fn questions(paths: &Paths) -> Result<Vec<Question>, CliError> {
    match ipc::run(paths, Request::Questions).await? {
        Response::Questions { questions } => Ok(questions),
        other => Err(unexpected(other)),
    }
}

/// The decision `id` once it has an answer, waiting for sync or taken by it, for an answer given again.
async fn answered_question(paths: &Paths, id: &str) -> Result<Option<Question>, CliError> {
    let request = Request::AnsweredQuestion {
        question_id: id.to_owned(),
    };
    match ipc::run(paths, request).await? {
        Response::AnsweredQuestion { question } => Ok(question),
        other => Err(unexpected(other)),
    }
}

/// `koi status`: this device, its folders and the decisions waiting in them. It succeeds whenever it reads the
/// status; with `check` it exits with the status of the most urgent condition (see [`report::check_exit`]).
async fn status(paths: &Paths, check: bool) -> Result<Done, CliError> {
    let status = match ipc::run(paths, Request::Status).await? {
        Response::Status { status } => status,
        other => return Err(unexpected(other)),
    };
    let questions = match status.folders.iter().any(|folder| folder.questions > 0) {
        true => questions(paths).await?,
        false => Vec::new(),
    };
    let conditions = report::conditions(&status, &questions);
    let exit = if check {
        report::check_exit(&conditions)
    } else {
        0
    };
    let service = service::inspect(paths, status.running).await;
    let version = (status.version.as_deref()).unwrap_or(env!("CARGO_PKG_VERSION"));
    let notice = match update::Install::current() {
        Ok(install) => install.notice(&status.update, version),
        Err(_) => UpdateNoticeData::of(&status.update, version, None),
    };
    let text = status_text(&status, &questions, &service.status_line(), &notice);
    let data = StatusData {
        account: status.account.as_ref().map(AccountData::of),
        running: status.running,
        paused: status.paused,
        folders: status.folders.iter().map(FolderData::of).collect(),
        conditions,
        service,
        update: notice,
    };
    let mut done = Done::ok(data, text);
    done.decisions = questions.iter().map(Decision::of).collect();
    done.exit = exit;
    if status.account.is_none() {
        done = done.then(&["koi", "setup"]);
    }
    if status.paused {
        done = done.then(&["koi", "resume"]);
    }
    Ok(done)
}

async fn pause(paths: &Paths) -> Result<Done, CliError> {
    match ipc::run(paths, Request::Pause).await? {
        Response::Paused => Ok(Done::ok(
            Empty {},
            "Paused sync. To sync again, run: koi resume\n",
        )),
        Response::AlreadyPaused => Ok(Done::ok(Empty {}, "Sync is already paused.\n").unchanged()),
        other => Err(unexpected(other)),
    }
}

async fn resume(paths: &Paths) -> Result<Done, CliError> {
    match ipc::run(paths, Request::Resume).await? {
        Response::Resumed => Ok(Done::ok(Empty {}, "Resumed sync.\n")),
        Response::NotPaused => Ok(Done::ok(Empty {}, "Sync is not paused.\n").unchanged()),
        other => Err(unexpected(other)),
    }
}

/// `koi resolve`: every pending decision, or the answer to one. An answer is recorded; sync carries it out later.
async fn resolve(
    paths: &Paths,
    id: Option<String>,
    word: Option<String>,
) -> Result<Done, CliError> {
    let questions = questions(paths).await?;
    let (Some(id), Some(word)) = (id, word) else {
        let mut done = Done::ok(Value::Null, questions_text(&questions));
        done.decisions = questions.iter().map(Decision::of).collect();
        return Ok(done);
    };
    let question = match questions.into_iter().find(|question| question.id == id) {
        Some(question) => question,
        None => answered_question(paths, &id)
            .await?
            .ok_or_else(|| DriverError::QuestionNotFound(id.clone()))?,
    };
    let answer = answer_to(&question, &word)
        .map_err(|message| CliError::usage(message).then(&["koi", "resolve", "--json"]))?;
    let request = Request::Answer {
        question_id: id.clone(),
        answer,
    };
    match ipc::run(paths, request).await? {
        Response::Answered => Ok(Done::accepted(
            AnsweredData {
                decision_id: id,
                answer: word,
            },
            "Resolved.\n",
        )),
        Response::AlreadyAnswered => Ok(Done::ok(
            AnsweredData {
                decision_id: id,
                answer: word.clone(),
            },
            format!("Already resolved with {}.\n", printable(&word)),
        )
        .unchanged()),
        other => Err(unexpected(other)),
    }
}

/// Runs a folder command in the daemon, or on its own when no daemon runs.
async fn folder(paths: &Paths, action: FolderAction) -> Result<Done, CliError> {
    let request = match action {
        FolderAction::Link { folder, space } => {
            let (folder, outcome) = link(paths, absolute(&folder)?, space, None).await?;
            let text = linked(&folder, outcome);
            return Ok(Done {
                outcome,
                ..Done::ok(FolderData::of(&folder), text)
            });
        }
        FolderAction::Unlink { folder } => Request::Unlink {
            folder: absolute(&folder)?,
        },
        FolderAction::Restart { folder, request } => {
            let folder = absolute(&folder)?;
            let key = Key::restart(&folder);
            Request::Restart { folder }.once(request.request_id, key)
        }
        FolderAction::Retry {
            folder,
            replan,
            request,
            ..
        } => {
            let folder = absolute(&folder)?;
            let key = Key::retry(&folder, replan);
            Request::Lift { folder, replan }.once(request.request_id, key)
        }
    };
    match ipc::run(paths, request).await? {
        Response::Unlinked { space_id, path } => {
            let text = format!(
                "Unlinked {} from space {}. Its files and its .koidrive folder stay.\n",
                printable(&path.display().to_string()),
                printable(&space_id)
            );
            let data = UnlinkedData {
                space_id: Some(space_id),
                path: path.display().to_string(),
            };
            Ok(Done::ok(data, text))
        }
        Response::NotLinked { path } => {
            let path = path.display().to_string();
            let text = format!(
                "{} is not linked, so there is nothing to unlink.\n",
                printable(&path)
            );
            let data = UnlinkedData {
                space_id: None,
                path,
            };
            Ok(Done::ok(data, text).unchanged())
        }
        Response::Restarted { folder } => restarted(folder),
        other => Err(unexpected(other)),
    }
}

async fn link(
    paths: &Paths,
    folder: PathBuf,
    space: String,
    approval: Option<koidrive_driver::LinkApproval>,
) -> Result<(FolderStatus, report::Outcome), CliError> {
    let request = match approval {
        Some(approval) => {
            service::reconcile(paths, false).await;
            Request::LinkApproved {
                folder,
                space,
                approval,
            }
        }
        None => Request::Link { folder, space },
    };
    match ipc::run(paths, request).await? {
        Response::Linked { folder } => Ok((folder, report::Outcome::Ok)),
        Response::AlreadyLinked { folder } => Ok((folder, report::Outcome::Unchanged)),
        other => Err(unexpected(other)),
    }
}

fn linked(folder: &FolderStatus, outcome: report::Outcome) -> String {
    let path = printable(&folder.path);
    let space = format!(
        "space {} ({})",
        printable(&folder.space_name),
        printable(&folder.space_id)
    );
    match outcome {
        report::Outcome::Unchanged => format!("{path} is already linked to {space}.\n"),
        _ => format!("Linked {path} to {space}.\n"),
    }
}

/// What `folder restart` reports for the folder as the restart left it.
fn restarted(folder: FolderStatus) -> Result<Done, CliError> {
    let path = printable(&folder.path);
    let text = match folder.state {
        FolderState::Halted => {
            let problem = printable(folder.problem.as_deref().unwrap_or_default());
            return Err(CliError::new(
                Code::Halted,
                format!("{path} cannot sync: {problem}"),
            ));
        }
        _ => format!("Syncing {path} again.\n"),
    };
    Ok(Done::ok(FolderData::of(&folder), text))
}

/// This profile's credential, for the commands that talk to the server directly. They write nothing to the profile, so
/// they run the same whether or not the daemon holds it.
fn signed_in(paths: &Paths) -> Result<Auth, CliError> {
    let credential = Credential::load(&paths.credential())
        .map_err(|error| error.to_string())?
        .ok_or(DriverError::NotPaired)?;
    Ok(reading(paths, &credential)?)
}

/// An [`Auth`] for a command that only reads the profile's credential. The process that holds the profile may rotate
/// the credential meanwhile, so when the server refuses it, it reads both credential files again, once, and presents
/// what they hold (RFC 0011 section 2). A reload that finds a credential for another server fails, so that no
/// credential reaches a server it was not issued by.
fn reading(paths: &Paths, credential: &Credential) -> Result<Auth, String> {
    let path = paths.credential();
    let server = credential.server.clone();
    let reload: Reload = Arc::new(move || {
        let stored = Credential::load_stored(&path)
            .map_err(io::Error::other)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, DriverError::NotPaired.to_string())
            })?;
        if stored.credential.server != server {
            return Err(io::Error::other(
                "the profile now holds a credential for another server",
            ));
        }
        Ok(stored.net())
    });
    Ok(Auth::reloading(
        credential.api().map_err(|error| error.to_string())?,
        credential.net(),
        reload,
    ))
}

fn left(not_removed: Vec<koidrive_driver::CleanupFailure>) -> Done {
    let mut text = "Signed out. This device no longer syncs, and the files in its folders stay where they are.\n".to_owned();
    for failure in &not_removed {
        text.push_str(&format!(
            "{:?} could not be removed ({}). Run koi device leave again to remove it.\n",
            failure.path,
            printable(&failure.error),
        ));
    }
    let data = LeftData {
        not_removed: (not_removed.into_iter())
            .map(|failure| NotRemovedData {
                path: failure.path.to_string_lossy().into_owned(),
                error: failure.error,
            })
            .collect(),
    };
    Done::ok(data, text)
}

/// Runs a device command (RFC 0009).
async fn device(paths: Paths, action: DeviceAction) -> Result<Done, CliError> {
    match action {
        DeviceAction::Leave { last_device } => {
            let request = Request::Leave {
                confirm_last_device: last_device,
            };
            let not_removed = match ipc::run(&paths, request).await? {
                Response::SignedOut { not_removed } => not_removed,
                other => return Err(unexpected(other)),
            };
            Ok(left(not_removed))
        }
        DeviceAction::List => {
            let auth = signed_in(&paths)?;
            let devices = devices(&auth).await?;
            let data = DevicesData {
                devices: devices.iter().map(DeviceData::of).collect(),
            };
            Ok(Done::ok(data, device_table(&devices, now_ms())))
        }
        DeviceAction::Invite { request } => {
            let auth = signed_in(&paths)?;
            let claim = match request.request_id {
                None => None,
                Some(id) => {
                    let key = Key::new(
                        "device invite",
                        serde_json::json!({
                            "server": auth.api().base().to_string(),
                            "device_id": auth.credentials().device_id,
                        }),
                    );
                    match request::begin(&paths, &id, &key)? {
                        Begun::Done(recorded) => {
                            let invitation = serde_json::from_value(recorded).map_err(|error| {
                                format!("the record of the request {id} cannot be read: {error}")
                            })?;
                            return replayed(&id, &invitation, now_ms());
                        }
                        Begun::Claimed(claim) => Some(claim),
                    }
                }
            };
            let started = match auth.api().pair_start(&auth).await {
                Ok(started) => started,
                Err(error) => {
                    if let Some(claim) = claim.filter(|_| refused(&error)) {
                        claim.abandon();
                    }
                    return Err(error.into());
                }
            };
            let invitation = Invitation {
                code: started.code,
                server: auth.api().base().to_string(),
                expires_at_ms: started.expires_at_ms,
            };
            if let Some(claim) = claim {
                claim.finish(&invitation)?;
            }
            invited(&invitation, now_ms())
        }
        DeviceAction::Revoke { id } => {
            let auth = signed_in(&paths)?;
            if id == auth.credentials().device_id {
                return Err(CliError::usage(
                    "this device cannot revoke itself; revoke it from another device",
                ));
            }
            let revoked = devices(&auth).await.is_ok_and(|devices| {
                (devices.iter()).any(|device| device.device_id == id && device.revoked_at_ms != 0)
            });
            if revoked {
                let text = format!("Device {id} is already revoked.\n");
                return Ok(Done::ok(RevokedData { device_id: id }, text).unchanged());
            }
            auth.api().revoke_device(&auth, &id).await?;
            let text = format!(
                "Revoked device {id}. It can no longer sync; the files already on it stay there.\n"
            );
            Ok(Done::ok(RevokedData { device_id: id }, text))
        }
    }
}

/// Whether the call changed nothing on the server: it was never sent, or the server refused it with a 4xx status. Without
/// an answer, or with a 5xx status, whether or not its error could be read, the call may have changed something.
fn refused(error: &ApiError) -> bool {
    match error {
        ApiError::Url(_) | ApiError::PathSegment(_) | ApiError::Credential(_) => true,
        ApiError::Server { status, .. }
        | ApiError::Status { status }
        | ApiError::Unreadable { status } => (400..500).contains(status),
        ApiError::Transport(_) | ApiError::TooLarge { .. } | ApiError::Decode(_) => false,
    }
}

/// Every device of the account, page by page. A page that repeats an earlier cursor ends the listing as undecodable, as
/// in `Api::list_spaces`.
async fn devices(auth: &Auth) -> Result<Vec<DeviceInfo>, CliError> {
    let mut devices = Vec::new();
    let mut cursor = String::new();
    let mut seen = std::collections::HashSet::new();
    loop {
        let page = auth.api().list_devices(auth, &cursor).await?;
        devices.extend(page.devices);
        if page.next_cursor.is_empty() {
            return Ok(devices);
        }
        if !seen.insert(page.next_cursor.clone()) {
            return Err(ApiError::Decode("DeviceList").into());
        }
        cursor = page.next_cursor;
    }
}

/// A pairing code `koi device invite` showed, as its request record keeps it.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Invitation {
    code: String,
    server: String,
    expires_at_ms: i64,
}

/// What `koi device invite` shows for `invitation`: the code on stdout, how to use it on stderr.
fn invited(invitation: &Invitation, now_ms: i64) -> Result<Done, CliError> {
    let server = url::Url::parse(&invitation.server).map_err(|error| error.to_string())?;
    let minutes = ((invitation.expires_at_ms - now_ms + 59_999) / 60_000).max(1);
    let code = pairing::join_args(&invitation.code, &server);
    let note = format!(
        "On the new device, run:\n  koi join {code}\nThe code works once, for about {minutes} minutes.\n"
    );
    let mut join_argv = vec!["koi".to_owned(), "join".to_owned()];
    join_argv.extend(pairing::join_argv(&invitation.code, &server));
    let data = InvitationData {
        join_argv,
        expires_at_ms: invitation.expires_at_ms,
    };
    Ok(Done::value(data, &code, note))
}

/// The invitation the request `id` showed before, or a stale request once it expired.
fn replayed(id: &RequestId, invitation: &Invitation, now_ms: i64) -> Result<Done, CliError> {
    if invitation.expires_at_ms <= now_ms {
        return Err(CliError::new(
            Code::StaleDecision,
            format!(
                "stale request: the pairing code of the request {id} expired; run koi device invite with a new \
                 request ID"
            ),
        ));
    }
    invited(invitation, now_ms)
}

/// Runs a space command. Creating a name another space has returns that space, also when another run creates it at the
/// same time, since koi folder link names a space by its name and the server keeps names unique.
async fn space(paths: Paths, action: SpaceAction) -> Result<Done, CliError> {
    let auth = signed_in(&paths)?;
    let spaces = auth.api().list_spaces(&auth).await?.spaces;
    match action {
        SpaceAction::List => {
            let data = SpacesData {
                spaces: spaces.iter().map(SpaceData::of).collect(),
            };
            Ok(Done::ok(data, space_table(&spaces)))
        }
        SpaceAction::Usage => usage(&auth, spaces).await,
        SpaceAction::Create { name } => {
            let name = name.trim();
            if name.is_empty() {
                return Err(CliError::usage("a space needs a name"));
            }
            let named = named_space(&auth, &spaces, name).await?;
            let space = named.space();
            let id = printable(&space.space_id);
            let shown = format!("{} ({id})", printable(&space.name));
            let said = match &named {
                Named::Existing(_) => format!("The space {shown} already exists."),
                Named::Created(_) => format!("Created the space {shown}."),
            };
            let note = format!(
                "{said}\nLink a folder to it with koi folder link <folder> --space {id}.\n"
            );
            let done = Done::value(SpaceData::of(space), &id, note);
            Ok(match named {
                Named::Existing(_) => done.unchanged(),
                Named::Created(_) => done,
            })
        }
    }
}

enum Named {
    Existing(SpaceInfo),
    Created(SpaceInfo),
}

impl Named {
    fn space(&self) -> &SpaceInfo {
        match self {
            Named::Existing(space) | Named::Created(space) => space,
        }
    }
}

async fn named_space(auth: &Auth, spaces: &[SpaceInfo], name: &str) -> Result<Named, CliError> {
    if let Some(space) = existing_space(spaces, name)? {
        return Ok(Named::Existing(space));
    }
    match create_space(auth, name).await {
        Ok(created) => Ok(Named::Created(created)),
        Err(error) if error.status() == Some(409) => {
            let now = auth.api().list_spaces(auth).await?;
            match existing_space(&now.spaces, name)? {
                Some(space) => Ok(Named::Existing(space)),
                None => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

async fn create_space(auth: &Auth, name: &str) -> Result<SpaceInfo, ApiError> {
    let request = CreateSpaceRequest {
        name: name.to_owned(),
        request_id: koidrive_net::request_id(),
    };
    koidrive_net::resend_lost(|| auth.api().create_space(auth, &request)).await
}

fn existing_space(spaces: &[SpaceInfo], name: &str) -> Result<Option<SpaceInfo>, CliError> {
    let matches: Vec<_> = spaces.iter().filter(|space| space.name == name).collect();
    match matches.as_slice() {
        [] => Ok(None),
        [space] => Ok(Some((*space).clone())),
        _ => Err(koidrive_driver::DriverError::SpaceAmbiguous {
            name: name.to_owned(),
            ids: matches
                .into_iter()
                .map(|space| space.space_id.clone())
                .collect(),
        }
        .into()),
    }
}

/// One line per space: its ID and its name.
fn space_table(spaces: &[SpaceInfo]) -> String {
    if spaces.is_empty() {
        return "No spaces. Create one with koi space create <name>.\n".to_owned();
    }
    let mut table = format!("{:<36}  NAME\n", "ID");
    for space in spaces {
        table.push_str(&format!(
            "{:<36}  {}\n",
            printable(&space.space_id),
            printable(&space.name)
        ));
    }
    table
}

/// Shows what each space of the account stores (RFC 0010). A space the server refuses, such as one a rewind holds
/// closed, does not hide the others: they are reported, and the command then fails naming the refused ones. A refused
/// credential stops it, since every later space would refuse it too, and the failure keeps what was read before.
async fn usage(auth: &Auth, spaces: Vec<SpaceInfo>) -> Result<Done, CliError> {
    let mut usages = Vec::new();
    let mut refused = Vec::new();
    for space in spaces {
        match auth.api().space_usage(auth, &space.space_id).await {
            Ok(usage) => usages.push((space, usage)),
            Err(error) if error.is_unauthenticated() => {
                let mut error = CliError::from(error);
                if !(usages.is_empty() && refused.is_empty()) {
                    error.partial = Some(Box::new(usage_read(&usages, &refused)));
                }
                return Err(error);
            }
            Err(error) => refused.push((space, CliError::from(error))),
        }
    }
    usage_done(&usages, &refused)
}

/// The usage of the spaces that answered, which fails naming the spaces the server refused. Those add nothing to the
/// data but their names: unavailable when each refusal may pass, failed otherwise.
fn usage_done(
    usages: &[(SpaceInfo, SpaceUsage)],
    refused: &[(SpaceInfo, CliError)],
) -> Result<Done, CliError> {
    let done = usage_read(usages, refused);
    if refused.is_empty() {
        return Ok(done);
    }
    let code = match refused
        .iter()
        .all(|(_, error)| error.code == Code::Unavailable)
    {
        true => Code::Unavailable,
        false => Code::Failed,
    };
    let names: Vec<String> = (refused.iter())
        .map(|(space, error)| {
            format!(
                "{} ({}): {}",
                printable(&space.name),
                space.space_id,
                error.message
            )
        })
        .collect();
    let mut error = CliError::new(code, format!("no usage for {}", names.join("; ")));
    error.partial = Some(Box::new(done));
    Err(error)
}

/// What was read: the usage of the spaces that answered and the names of those the server refused.
fn usage_read(usages: &[(SpaceInfo, SpaceUsage)], refused: &[(SpaceInfo, CliError)]) -> Done {
    let data = UsagesData {
        spaces: (usages.iter())
            .map(|(space, usage)| UsageData::of(space, usage))
            .collect(),
        refused: (refused.iter())
            .map(|(space, error)| RefusedSpace {
                space_id: space.space_id.clone(),
                name: space.name.clone(),
                message: error.message.clone(),
            })
            .collect(),
    };
    Done::ok(data, usage_output(usages, refused.len()))
}

/// What `koi space usage` prints on stdout: the spaces that answered. It prints nothing when every space refused, so
/// that stdout never shows an empty account that is not empty; the error names them instead.
fn usage_output(usages: &[(SpaceInfo, SpaceUsage)], refused: usize) -> String {
    if usages.is_empty() && refused > 0 {
        return String::new();
    }
    usage_report(usages)
}

fn total_bytes(usage: &SpaceUsage) -> u64 {
    usage.live_bytes + usage.trash_bytes + usage.old_version_bytes + usage.unreferenced_bytes
}

/// One block per space: its live files, trash, old versions and unreferenced chunks, then a note on how they count.
fn usage_report(usages: &[(SpaceInfo, SpaceUsage)]) -> String {
    if usages.is_empty() {
        return "No spaces.\n".to_owned();
    }
    let mut report = String::new();
    for (space, usage) in usages {
        let rows = [
            (
                "Live files",
                count(usage.live_files, "file"),
                usage.live_bytes,
            ),
            (
                "Trash",
                count(usage.trashed_files, "file"),
                usage.trash_bytes,
            ),
            (
                "Old versions",
                count(usage.old_versions, "version"),
                usage.old_version_bytes,
            ),
            ("Unreferenced", String::new(), usage.unreferenced_bytes),
            ("Total", String::new(), total_bytes(usage)),
        ];
        report.push_str(&format!(
            "{} ({})\n",
            printable(&space.name),
            space.space_id
        ));
        for (label, what, bytes) in rows {
            report.push_str(&format!("  {label:<14}{what:>14}  {:>9}\n", size(bytes)));
        }
    }
    report.push_str(
        "\nEach stored chunk counts once, under the first of live files, trash and old versions that holds it.\n\
         Unreferenced chunks are uploads not committed yet and chunks waiting out the grace period before deletion.\n",
    );
    report
}

fn count(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// How many decisions wait for the person, as summary lines say it.
fn need_you(n: u64) -> String {
    format!("{n} {}", if n == 1 { "needs you" } else { "need you" })
}

/// Bytes in decimal units, such as 999 B, 1.0 kB or 1.5 GB.
fn size(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    for unit in ["kB", "MB", "GB", "TB"] {
        value /= 1000.0;
        if value < 999.95 {
            return format!("{value:.1} {unit}");
        }
    }
    format!("{:.1} PB", value / 1000.0)
}

/// A space, device or user name as the server takes it: at most 255 bytes of UTF-8, measured before trimming (RFC 0007
/// section 1). Checking it here names the argument instead of sending a request the server refuses.
fn short_name(text: &str) -> Result<String, String> {
    const MAX_NAME: usize = 255;
    if text.len() > MAX_NAME {
        return Err(format!(
            "a name is at most {MAX_NAME} bytes of UTF-8, and this one has {}",
            text.len()
        ));
    }
    Ok(text.to_owned())
}

/// A device ID as the server issues it: a lowercase UUID. Checked here so that the argument cannot name another path.
fn device_id(text: &str) -> Result<String, String> {
    let id = text.to_ascii_lowercase();
    let shaped = id.len() == 36
        && id.char_indices().all(|(at, c)| match at {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    if shaped {
        Ok(id)
    } else {
        Err("not a device ID; koi device list shows them".to_owned())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// One line per device: its ID, name, platform, when it last refreshed a token, and whether it is this device, active
/// or revoked.
fn device_table(devices: &[DeviceInfo], now_ms: i64) -> String {
    let rows: Vec<[String; 5]> = devices
        .iter()
        .map(|device| {
            let state = match (device.revoked_at_ms, device.current) {
                (0, true) => "this device".to_owned(),
                (0, false) => "active".to_owned(),
                (revoked, _) => format!("revoked {}", ago(revoked, now_ms)),
            };
            [
                printable(&device.device_id),
                printable(&device.name),
                printable(&device.platform),
                ago(device.last_seen_ms, now_ms),
                state,
            ]
        })
        .collect();
    let header = ["ID", "NAME", "PLATFORM", "LAST SEEN", "STATE"].map(str::to_owned);
    let mut widths = [0; 5];
    for row in std::iter::once(&header).chain(&rows) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut table = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let cells: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        table.push_str(cells.join("  ").trim_end());
        table.push('\n');
    }
    table
}

/// The text with control characters and bidirectional formatting characters escaped, such as `\u{1b}`. Another device
/// chose its name and platform, and a terminal would run the escape sequences in them or show forged rows.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                c.escape_unicode().to_string()
            }
            c if c.is_control() => c.escape_unicode().to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// How long before `now_ms` a time in Unix milliseconds was, or "never" for 0.
fn ago(ms: i64, now_ms: i64) -> String {
    if ms == 0 {
        return "never".to_owned();
    }
    let minutes = (now_ms - ms).max(0) / 60_000;
    match minutes {
        0 => "just now".to_owned(),
        1..60 => format!("{minutes} min ago"),
        60..2880 => format!("{} h ago", minutes / 60),
        _ => format!("{} days ago", minutes / 1440),
    }
}

/// The folder as an absolute path, since the daemon does not share this command's working directory.
fn absolute(folder: &Path) -> Result<PathBuf, CliError> {
    std::path::absolute(folder).map_err(|error| {
        CliError::usage(format!(
            "{}: {error}",
            printable(&folder.display().to_string())
        ))
    })
}

fn status_text(
    status: &Status,
    questions: &[Question],
    service: &str,
    notice: &UpdateNoticeData,
) -> String {
    let mut text = match &status.account {
        Some(account) => format!(
            "Device: {} ({}) on {}\n",
            printable(&account.device_name),
            printable(&account.device_id),
            printable(&account.server)
        ),
        None => "Device: not set up; run koi setup\n".to_owned(),
    };
    text.push_str(&format!(
        "Daemon: {}\n",
        if status.running {
            "running"
        } else {
            "not running"
        }
    ));
    text.push_str(service);
    if let Some(latest) = &notice.latest {
        text.push_str(&format!(
            "Update: Koidrive {} is out (this device runs {}).",
            printable(latest),
            printable(&notice.version),
        ));
        if let Some(command) = &notice.command {
            text.push_str(&format!(" Run: {}\n", printable(command)));
        } else if let Some(url) = &notice.url {
            text.push_str(&format!(" Download: {}\n", printable(url)));
        } else {
            text.push('\n');
        }
    }
    if status.paused {
        text.push_str("Sync: paused. To sync again, run: koi resume\n");
    }
    if status.folders.is_empty() {
        text.push_str("No linked folders; run koi folder link.\n");
    }
    for folder in &status.folders {
        text.push_str(&folder_text(folder));
        for question in (questions.iter()).filter(|question| question.space_id == folder.space_id) {
            text.push_str(&question_brief(question));
        }
    }
    text
}

fn folder_text(folder: &FolderStatus) -> String {
    let seq = match (folder.last_seq, folder.head_seq) {
        (Some(last), Some(head)) => format!("seq {last} of {head}"),
        (Some(last), None) => format!("seq {last}"),
        _ => "not read yet".to_owned(),
    };
    let state = match folder.state {
        FolderState::Stopped => "Stopped",
        FolderState::Connecting => "Connecting",
        FolderState::Bootstrapping => "Bootstrapping",
        FolderState::CatchingUp => "Syncing",
        FolderState::UpToDate => "UpToDate",
        FolderState::Offline => "Offline",
        FolderState::Halted => "Halted",
    };
    let mut text = format!(
        "{}  space {} ({})  {state}  {seq}, {} remote nodes{}, {}\n",
        printable(&folder.path),
        printable(&folder.space_name),
        printable(&folder.space_id),
        folder.remote_nodes,
        folder
            .local_entries
            .map(|entries| format!(", {entries} local entries"))
            .unwrap_or_default(),
        need_you(folder.questions)
    );
    if let Some(progress) = &folder.progress {
        text.push_str(&format!("  {}\n", progress_text(progress)));
    }
    if let Some(problem) = &folder.problem {
        text.push_str(&format!("  {}\n", printable(problem)));
    }
    if let Some(cause) = &folder.recovery {
        let described = koidrive_driver::describe(cause);
        if folder.problem.as_deref() != Some(described.as_str()) {
            text.push_str(&format!("  {described}\n"));
        }
    }
    text
}

fn progress_text(progress: &Progress) -> String {
    let files = |count: u64, way: &str| match count {
        0 => None,
        1 => Some(format!("1 file to {way}")),
        count => Some(format!("{count} files to {way}")),
    };
    let mut parts: Vec<String> = [
        files(progress.files_to_upload, "upload"),
        files(progress.files_to_download, "download"),
    ]
    .into_iter()
    .flatten()
    .collect();
    parts.push(format!(
        "{} of {}",
        size(progress.bytes_done),
        size(progress.bytes_total)
    ));
    if let Some(current) = &progress.current {
        parts.push(format!("now {}", printable(current)));
    }
    parts.join("; ")
}

fn questions_text(questions: &[Question]) -> String {
    if questions.is_empty() {
        return "No decisions are waiting.\n".to_owned();
    }
    questions.iter().map(question_text).collect()
}

/// The command that lists and answers decisions, as every message names it.
const RESOLVE: &str = "koi resolve";

/// The most items `koi resolve` lists for one decision.
const ITEMS_SHOWN: usize = 20;

/// What a decision is about, in one line.
fn question_summary(question: &Question) -> String {
    let why = |exceeded: &[Limit]| {
        let limits: Vec<String> = (exceeded.iter())
            .map(|limit| match limit {
                Limit::Deletions => format!("more than {MAX_DELETIONS} deletions in 10 minutes"),
                Limit::Share => "more than half of the space in 10 minutes".to_owned(),
            })
            .collect();
        match &limits[..] {
            [] => String::new(),
            limits => format!(", {}", limits.join(" and ")),
        }
    };
    match &question.kind {
        QuestionKind::MassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded,
        } => format!(
            "held {deletions} deletions that would remove {affected_files} of {space_files} files{}",
            why(exceeded)
        ),
        QuestionKind::RemoteMassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded,
        } => format!(
            "applied {deletions} deletions from other devices that removed {affected_files} of \
             {space_files} files{}",
            why(exceeded)
        ),
        QuestionKind::AmbiguousDisappearance => {
            "files disappeared while changes were not tracked".to_owned()
        }
        QuestionKind::Recovery { group, .. } => match group {
            RecoveryGroup::CreatedAfterRestorePoint => {
                "files created after the restore point, which the restore removed from the server"
            }
            RecoveryGroup::ChangedOnThisDevice => {
                "files changed on this device since it last synced"
            }
            RecoveryGroup::CreatedOnThisDevice => {
                "files on this device that the space does not hold"
            }
            RecoveryGroup::DeletedOnThisDevice => {
                "files deleted on this device that the server still holds"
            }
            RecoveryGroup::RequestedBeforeReset => {
                "restores asked for before recovery began, which the server does not show"
            }
        }
        .to_owned(),
        QuestionKind::Conflict { original, .. } => format!(
            "{} has a conflict copy",
            conflict_path(question, &original.node_id)
        ),
    }
}

/// The first path a decision holds, after its summary, or nothing for a decision that holds none or a conflict, whose
/// summary names its file.
fn first_path(question: &Question) -> String {
    if let QuestionKind::Conflict { .. } = question.kind {
        return String::new();
    }
    (question.items.first())
        .map(|item| format!("; first path: {}", printable(&item.path)))
        .unwrap_or_default()
}

/// A file of a conflict as a shell word with its control characters escaped, or its node ID if the decision does not
/// list it.
fn conflict_path(question: &Question, node_id: &str) -> String {
    (question.items.iter())
        .find(|item| item.node_id == node_id)
        .map(|item| printable(&pairing::shell_word(&item.path)))
        .unwrap_or_else(|| format!("node {}", printable(node_id)))
}

/// The two files of a conflict, each with the device that wrote its current version, or nothing for another decision.
fn conflict_files(question: &Question, indent: &str) -> String {
    let QuestionKind::Conflict { original, copy, .. } = &question.kind else {
        return String::new();
    };
    [("Original", original), ("Conflict copy", copy)]
        .into_iter()
        .map(|(role, file)| {
            let writer = (question.items.iter())
                .find(|item| item.node_id == file.node_id)
                .and_then(|item| item.device_id.as_deref())
                .map_or("an unknown device".to_owned(), |device_id| {
                    format!("device {}", printable(device_id))
                });
            format!(
                "{indent}{role}: {}, written by {writer}\n",
                conflict_path(question, &file.node_id)
            )
        })
        .collect()
}

/// The word `koi resolve` takes for a conflict's answer that keeps both files.
const KEEP_BOTH: &str = "keep-both";

/// One answer as `koi resolve` offers it: the word that gives it, what it does, and the engine's answer it stands
/// for.
struct Choice {
    word: String,
    effect: String,
    answer: Answer,
}

/// The answers a decision takes, in the order to offer them. A conflict's answers name the node ID of the file to keep,
/// so that no one has to tell which device's file is "mine".
fn choices(question: &Question) -> Vec<Choice> {
    let QuestionKind::Conflict { original, copy, .. } = &question.kind else {
        return (question.kind.answers().iter())
            .map(|&answer| Choice {
                word: answer.as_str().to_owned(),
                effect: answer_effect(answer).to_owned(),
                answer,
            })
            .collect();
    };
    let kept = conflict_path(question, &original.node_id);
    let copied = conflict_path(question, &copy.node_id);
    vec![
        Choice {
            word: format!("keep-node:{}", original.node_id),
            effect: format!("Keep {kept} and move {copied} to the trash"),
            answer: Answer::KeepOriginal,
        },
        Choice {
            word: format!("keep-node:{}", copy.node_id),
            effect: format!(
                "Give {kept} the content of {copied}, keeping the old content in its version history, and move \
                 {copied} to the trash"
            ),
            answer: Answer::KeepCopy,
        },
        Choice {
            word: KEEP_BOTH.to_owned(),
            effect: "Keep both files".to_owned(),
            answer: Answer::KeepBoth,
        },
    ]
}

/// The engine's answer that `word` gives `question`, or an error that lists the words it takes.
fn answer_to(question: &Question, word: &str) -> Result<Answer, String> {
    let choices = choices(question);
    if let Some(choice) = choices.iter().find(|choice| choice.word == word) {
        return Ok(choice.answer);
    }
    let words: Vec<String> = (choices.iter())
        .map(|choice| printable(&choice.word))
        .collect();
    Err(format!(
        "{} is not an answer to {}; use {}",
        printable(word),
        printable(&question.id),
        words.join(" or ")
    ))
}

/// Each answer of a decision: what it does, and under it the full command that gives it.
fn resolve_commands(question: &Question, indent: &str) -> String {
    let id = printable(&question.id);
    (choices(question).iter())
        .map(|choice| {
            format!(
                "{indent}{}:\n{indent}  {RESOLVE} {id} {}\n",
                choice.effect,
                printable(&choice.word)
            )
        })
        .collect()
}

/// What an answer to a decision other than a conflict does.
fn answer_effect(answer: Answer) -> &'static str {
    match answer {
        Answer::Delete => "Delete these files on the server and other devices",
        Answer::Restore => "Download these files to this device again",
        Answer::Keep => "Put these files back on the server",
        Answer::Discard => "Move this device's copies to the local trash and follow the server",
        Answer::Publish => "Send this device's version to the server",
        Answer::Repeat => "Send the request again",
        Answer::Cancel => "Drop the request",
        Answer::Dismiss => "Close this notice",
        Answer::KeepOriginal | Answer::KeepCopy | Answer::KeepBoth => {
            unreachable!("a conflict's answers name its files")
        }
    }
}

/// How `koi status` shows a decision under its folder: what it is about, its first path, and how to make it.
fn question_brief(question: &Question) -> String {
    format!(
        "  {}: {}{}\n{}{}",
        printable(&question.id),
        question_summary(question),
        first_path(question),
        conflict_files(question, "    "),
        resolve_commands(question, "  ")
    )
}

/// How `koi resolve` shows a decision: what it is about, how to make it, and its first [`ITEMS_SHOWN`] items.
fn question_text(question: &Question) -> String {
    let mut text = format!(
        "{}  space {}  {}\n{}{}",
        printable(&question.id),
        printable(&question.space_id),
        question_summary(question),
        conflict_files(question, "    "),
        resolve_commands(question, "  ")
    );
    if let QuestionKind::Conflict { .. } = question.kind {
        return text;
    }
    for item in question.items.iter().take(ITEMS_SHOWN) {
        text += &format!(
            "  {} ({})\n",
            printable(&item.path),
            count(item.files, "file")
        );
    }
    let more = question.items.len().saturating_sub(ITEMS_SHOWN);
    if more > 0 {
        text += &format!("  and {more} more; {RESOLVE} --json lists them all\n");
    }
    text
}

/// The text of a desktop notification for a decision: what it is about, its first path, and the command to run.
fn question_notice(question: &Question) -> String {
    format!(
        "{}{}. Run {RESOLVE}.",
        question_summary(question),
        first_path(question)
    )
}

/// The line every command but `status`, `resolve` and `daemon` prints on stderr while decisions are pending.
fn banner(questions: &[Question]) -> Option<String> {
    let n = questions.len() as u64;
    (n > 0).then(|| format!("koi: {}; run {RESOLVE}", need_you(n)))
}

/// The decisions pending in the profile, asked of the daemon. Without a daemon there are none to report, because a
/// command run on its own leaves the profile as it found it.
async fn pending_questions(paths: &Paths) -> Vec<Question> {
    let asked = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        koidrive_ipc::ask(paths, &Request::Questions),
    );
    match asked.await {
        Ok(Ok(Some(Response::Questions { questions }))) => questions,
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koidrive_net::ApiError;
    use koidrive_proto::v1::device_info::Kind;

    const ID: &str = "0190a0b0-0000-7000-8000-000000000001";

    fn parse(args: &[&str]) -> Result<Option<Command>, clap::Error> {
        Cli::try_parse_from(args).map(|cli| cli.command)
    }

    /// Answers one request per entry of `replies`, a status and a body, on a loopback port, in order, and returns the
    /// server's URL and, once all are answered, the path of each request.
    async fn serve(replies: Vec<(u16, Vec<u8>)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let mut paths = Vec::new();
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let read = socket.read(&mut request).await.unwrap();
                let line = String::from_utf8_lossy(&request[..read])
                    .lines()
                    .next()
                    .unwrap()
                    .to_owned();
                paths.push(line.split(' ').nth(1).unwrap().to_owned());
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
            paths
        });
        (server, served)
    }

    fn token() -> (u16, Vec<u8>) {
        let token = koidrive_proto::v1::AccessToken {
            token: "token".into(),
            expires_at_ms: i64::MAX,
            ..Default::default()
        };
        (200, prost::Message::encode_to_vec(&token))
    }

    /// Saves a credential of `device_id` for `server` in `paths`, in place of any other.
    fn signed_in_as(paths: &Paths, server: &str, device_id: &str) {
        let _ = std::fs::remove_file(paths.credential());
        Credential {
            server: server.into(),
            user_id: "u1".into(),
            user_name: None,
            device_id: device_id.into(),
            device_name: "vm".into(),
            refresh_credential: "secret".into(),
        }
        .save_new(&paths.credential())
        .unwrap();
    }

    #[tokio::test]
    async fn the_device_list_stops_at_a_cursor_it_has_seen() {
        let page = |cursor: &str| {
            let page = koidrive_proto::v1::DeviceList {
                devices: vec![DeviceInfo {
                    device_id: cursor.into(),
                    ..Default::default()
                }],
                next_cursor: cursor.into(),
            };
            (200, prost::Message::encode_to_vec(&page))
        };
        let (server, served) = serve(vec![token(), page("a"), page("b"), page("a")]).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        signed_in_as(&paths, &server, "d1");
        let auth = signed_in(&paths).unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), devices(&auth))
            .await
            .expect("the listing ended");
        assert_eq!(outcome.unwrap_err().code, Code::Failed);
        assert_eq!(served.await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn a_space_another_run_created_first_is_unchanged() {
        let list = |spaces: Vec<SpaceInfo>| {
            let list = koidrive_proto::v1::SpaceList {
                spaces,
                ..Default::default()
            };
            (200, prost::Message::encode_to_vec(&list))
        };
        let taken = koidrive_proto::v1::Error {
            code: koidrive_proto::v1::error::Code::InvalidArgument as i32,
            message: "you already have a space with this name".into(),
            ..Default::default()
        };
        let notes = SpaceInfo {
            space_id: ID.into(),
            name: "Notes".into(),
            ..Default::default()
        };
        let (server, served) = serve(vec![
            token(),
            list(Vec::new()),
            (409, prost::Message::encode_to_vec(&taken)),
            list(vec![notes]),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        signed_in_as(&paths, &server, "d1");
        let create = SpaceAction::Create {
            name: "Notes".into(),
        };
        let done = space(paths, create).await.unwrap();
        assert_eq!(done.outcome, crate::report::Outcome::Unchanged);
        assert_eq!(done.text, format!("{ID}\n"));
        assert_eq!(
            served.await.unwrap(),
            ["/v1/token", "/v1/spaces", "/v1/spaces", "/v1/spaces"]
        );
    }

    #[tokio::test]
    async fn a_replayed_invitation_belongs_to_the_device_that_asked_for_it() {
        let started = koidrive_proto::v1::PairStartResponse {
            code: "K7QM-2XPD".into(),
            expires_at_ms: i64::MAX,
        };
        let started = (200, prost::Message::encode_to_vec(&started));
        let (server, served) = serve(vec![token(), started]).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let invite = || {
            device(
                paths.clone(),
                DeviceAction::Invite {
                    request: RequestArg {
                        request_id: Some(RequestId::parse("r1").unwrap()),
                    },
                },
            )
        };
        signed_in_as(&paths, &server, "d1");
        let first = invite().await.unwrap();
        assert_eq!(invite().await.unwrap().text, first.text);
        signed_in_as(&paths, &server, "d2");
        let other = invite().await.unwrap_err();
        assert_eq!(other.code, Code::StaleDecision, "{other:?}");
        assert_eq!(served.await.unwrap(), ["/v1/token", "/v1/pair/start"]);
    }

    #[test]
    fn a_replayed_invitation_shows_the_first_code_until_it_expires() {
        let invitation = Invitation {
            code: "K7QM-2XPD".into(),
            server: "https://files.example.com/".into(),
            expires_at_ms: 600_000,
        };
        let id = RequestId::parse("r1").unwrap();
        let first = invited(&invitation, 0).unwrap();
        assert_eq!(first.text, "K7QM-2XPD@files.example.com\n");
        let again = replayed(&id, &invitation, 599_999).unwrap();
        assert_eq!(
            (again.outcome, &again.data, &again.text),
            (first.outcome, &first.data, &first.text)
        );
        let expired = replayed(&id, &invitation, 600_000).unwrap_err();
        assert_eq!(expired.code, Code::StaleDecision);
        assert_eq!(
            expired.message,
            "stale request: the pairing code of the request r1 expired; run koi device invite with a new request ID"
        );
        assert!(expired.next.is_empty());
    }

    #[test]
    fn only_a_refusal_the_server_sent_counts_as_no_change() {
        let refusal = ApiError::Server {
            status: 429,
            error: koidrive_net::ServerError(koidrive_proto::v1::Error::default()),
        };
        assert!(refused(&refusal));
        assert!(refused(&ApiError::Status { status: 403 }));
        assert!(refused(&ApiError::Unreadable { status: 400 }));
        let transport = reqwest::Client::new().get("not a url").build().unwrap_err();
        let internal = koidrive_proto::v1::Error {
            code: koidrive_proto::v1::error::Code::Internal as i32,
            ..Default::default()
        };
        for unknown in [
            ApiError::Server {
                status: 500,
                error: koidrive_net::ServerError(internal),
            },
            ApiError::Transport(transport),
            ApiError::Status { status: 504 },
            ApiError::Unreadable { status: 502 },
            ApiError::Decode("PairStartResponse"),
            ApiError::TooLarge { limit: 1 },
        ] {
            assert!(!refused(&unknown), "{unknown:?}");
        }
    }

    #[test]
    fn request_ids_are_checked_where_they_are_parsed() {
        assert!(parse(&["koi", "folder", "restart", "/a", "--request-id", "r-1_a"]).is_ok());
        assert!(parse(&["koi", "device", "invite", "--request-id", "r1"]).is_ok());
        assert!(parse(&["koi", "history", "restore", "a.txt", "--request-id", "r1"]).is_ok());
        let error = parse(&[
            "koi",
            "folder",
            "retry",
            "/a",
            "--resend",
            "--request-id",
            "../r1",
        ])
        .err()
        .unwrap();
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn the_status_text_escapes_every_name_path_and_problem() {
        let folder = FolderStatus {
            recovery: None,
            space_id: "s\u{1b}[2J".into(),
            space_name: "Home\u{202e}gpj.exe".into(),
            path: "/home/ada/\u{1b}]0;owned\u{7}Docs\nFake: up to date".into(),
            state: FolderState::Halted,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 0,
            problem: Some("cannot read \u{1b}[31mred\u{9b}31m".into()),
            progress: None,
            wait: None,
        };
        let status = Status {
            account: Some(koidrive_driver::Account {
                server: "https://files.example.com/\u{1b}[1A".into(),
                user_id: "u1".into(),
                user_name: None,
                device_id: "d\r1".into(),
                device_name: "Ada\u{1b}[8m".into(),
            }),
            running: true,
            folders: vec![folder],
            ..Status::default()
        };
        let notice = UpdateNoticeData::of(&status.update, "0.1.0", Some("koi update"));
        let text = status_text(&status, &[], "", &notice);
        for raw in ['\u{1b}', '\u{7}', '\u{9b}', '\u{202e}', '\r'] {
            assert!(
                !text.contains(raw),
                "{raw:?} reached the terminal: {text:?}"
            );
        }
        assert_eq!(text.lines().count(), 4, "{text}");
        assert!(text.contains("Ada\\u{1b}[8m"), "{text}");
        assert!(text.contains("Docs\\u{a}Fake"), "{text}");
        assert!(text.contains("red\\u{9b}31m"), "{text}");
    }

    #[test]
    fn the_status_text_names_the_rewind_a_folder_recovers_from() {
        let folder = FolderStatus {
            recovery: Some(koidrive_engine::question::RecoveryCause::rewind(
                7,
                1_794_442_424_000,
            )),
            progress: None,
            space_id: "s1".into(),
            space_name: "Home".into(),
            path: "/home/ada/Docs".into(),
            state: FolderState::UpToDate,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 1,
            problem: None,
            wait: None,
        };
        assert!(
            folder_text(&folder).ends_with(
                "\n  the space was rewound on the server to seq 7 (requested 2026-11-12 00:13 UTC)\n"
            ),
            "{}",
            folder_text(&folder)
        );
    }

    #[test]
    fn the_status_text_names_the_rewind_beside_a_problem_and_only_once() {
        let cause = koidrive_engine::question::RecoveryCause::rewind(7, 1_794_442_424_000);
        let rewound =
            "the space was rewound on the server to seq 7 (requested 2026-11-12 00:13 UTC)";
        let folder = |problem: &str| FolderStatus {
            recovery: Some(cause),
            progress: None,
            space_id: "s1".into(),
            space_name: "Home".into(),
            path: "/home/ada/Docs".into(),
            state: FolderState::UpToDate,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 1,
            problem: Some(problem.into()),
            wait: None,
        };
        let text = folder_text(&folder("a.txt waits to be read again"));
        assert!(
            text.ends_with(&format!("\n  a.txt waits to be read again\n  {rewound}\n")),
            "{text}"
        );
        let text = folder_text(&folder(rewound));
        assert_eq!(text.matches(rewound).count(), 1, "{text}");
    }

    #[test]
    fn logs_carry_colours_only_on_a_terminal_and_never_under_json() {
        assert!(log_ansi(true, false));
        assert!(!log_ansi(true, true));
        assert!(!log_ansi(false, false));
        assert!(!log_ansi(false, true));
    }

    #[test]
    fn the_restart_report_escapes_the_folder_and_its_problem() {
        let folder = |state, problem: Option<&str>| FolderStatus {
            recovery: None,
            space_id: "s1".into(),
            space_name: "Home".into(),
            path: "/home/ada/Docs\u{1b}[31m\nFORGED".into(),
            state,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 0,
            problem: problem.map(str::to_owned),
            progress: None,
            wait: None,
        };
        for state in [FolderState::Stopped, FolderState::CatchingUp] {
            let text = restarted(folder(state, None)).unwrap().text;
            assert!(!text.contains('\u{1b}'), "{text:?}");
            assert_eq!(text.lines().count(), 1, "{text:?}");
            assert!(text.contains("Docs\\u{1b}[31m\\u{a}FORGED"), "{text:?}");
        }
        let error = restarted(folder(FolderState::Halted, Some("gone\n\u{7}"))).unwrap_err();
        assert_eq!(error.code, Code::Halted);
        assert!(
            !error.message.contains(['\u{1b}', '\n', '\u{7}']),
            "{:?}",
            error.message
        );
    }

    #[test]
    fn codes_from_stdin_replace_but_never_accompany_positional_codes() {
        for prefix in [
            vec!["koi", "join"],
            vec![
                "koi",
                "server",
                "claim",
                "files.example.com",
                "--user",
                "Ada",
            ],
        ] {
            let mut args = prefix.clone();
            args.push("--code-stdin");
            assert!(parse(&args).is_ok(), "{args:?}");
            args.push("ABCD-EFGH");
            assert_eq!(
                parse(&args).err().unwrap().kind(),
                ErrorKind::ArgumentConflict,
                "{args:?}"
            );
            assert_eq!(
                parse(&prefix).err().unwrap().kind(),
                ErrorKind::MissingRequiredArgument,
                "{prefix:?}"
            );
            let mut positional = prefix;
            positional.push("ABCD-EFGH");
            assert!(parse(&positional).is_ok(), "{positional:?}");
        }
    }

    #[tokio::test]
    async fn leave_cleanup_quotes_terminal_controls_but_preserves_json_data() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let path = "/cleanup/line\n\u{1b}[2J";
        let error = "denied\n\u{1b}[31m";
        let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            koidrive_ipc::read_line(&mut stream).await.unwrap();
            koidrive_ipc::write_line(
                &mut stream,
                &Response::SignedOut {
                    not_removed: vec![koidrive_driver::CleanupFailure {
                        path: path.into(),
                        error: error.into(),
                    }],
                },
            )
            .await
            .unwrap();
        });

        let done = device(paths, DeviceAction::Leave { last_device: false })
            .await
            .unwrap();
        served.await.unwrap();

        assert!(done.text.contains("\"/cleanup/line\\n\\u{1b}[2J\""));
        assert!(done.text.contains("denied\\u{a}\\u{1b}[31m"));
        assert_eq!(done.text.lines().count(), 2);
        assert!(!done.text.contains('\u{1b}'));
        assert_eq!(done.data["not_removed"][0]["path"], path);
        assert_eq!(done.data["not_removed"][0]["error"], error);
        let encoded = serde_json::to_vec(&Envelope::of("device leave", done)).unwrap();
        let decoded: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded["data"]["not_removed"][0]["path"], path);
        assert_eq!(decoded["data"]["not_removed"][0]["error"], error);
    }

    #[test]
    fn leave_cleanup_accepts_non_utf8_paths_and_emits_lossy_json_without_panicking() {
        use std::os::unix::ffi::OsStringExt;

        let done = left(vec![koidrive_driver::CleanupFailure {
            path: PathBuf::from(OsString::from_vec(b"/cleanup/bad\xff\n\x1b".to_vec())),
            error: "denied\n\u{1b}".into(),
        }]);

        assert!(done.text.contains("\"/cleanup/bad\\xFF\\n\\u{1b}\""));
        assert!(done.text.contains("denied\\u{a}\\u{1b}"));
        assert_eq!(done.text.lines().count(), 2);
        assert!(!done.text.contains('\u{1b}'));
        assert_eq!(
            done.data["not_removed"][0]["path"],
            "/cleanup/bad\u{fffd}\n\u{1b}"
        );
        assert_eq!(done.data["not_removed"][0]["error"], "denied\n\u{1b}");
        let encoded = serde_json::to_vec(&Envelope::of("device leave", done)).unwrap();
        let decoded: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            decoded["data"]["not_removed"][0]["path"],
            "/cleanup/bad\u{fffd}\n\u{1b}"
        );
        assert_eq!(decoded["data"]["not_removed"][0]["error"], "denied\n\u{1b}");
    }

    #[tokio::test]
    async fn leaving_retries_cleanup_without_requiring_a_paired_account() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let line = koidrive_ipc::read_line(&mut stream).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Request>(&line).unwrap(),
                Request::Leave {
                    confirm_last_device: false,
                }
            );
            koidrive_ipc::write_line(
                &mut stream,
                &Response::Error {
                    code: Code::Failed,
                    message: "cleanup pending".into(),
                    reason: None,
                    next: Vec::new(),
                },
            )
            .await
            .unwrap();
        });
        let error = device(paths, DeviceAction::Leave { last_device: false })
            .await
            .err()
            .unwrap();
        served.await.unwrap();
        assert_eq!(error.message, "cleanup pending");
    }

    #[tokio::test]
    async fn resume_without_a_daemon_reconciles_without_overriding_background_off() {
        let (_dir, paths, _root) = ipc::tests::linked_profile();
        std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        std::fs::write(&paths.config_file, "[service]\nbackground = false\n").unwrap();
        assert_eq!(
            ipc::run(&paths, Request::Pause).await.unwrap(),
            Response::Paused
        );
        assert!(!paths.socket().exists());
        let validator = jsonschema::validator_for(&schema::envelope("resume").unwrap()).unwrap();

        for expected in [report::Outcome::Ok, report::Outcome::Unchanged] {
            let done = run(
                paths.clone(),
                Command::Resume,
                true,
                Arc::new(terminal::Terminal::new(true, false)),
            )
            .await
            .unwrap();
            assert_eq!(done.outcome, expected);
            assert_eq!(done.data["service"]["state"], "off");
            assert_eq!(done.data["service"]["folders"], 1);
            assert!(!paths.paused().exists());
            assert!(!paths.socket().exists());
            let envelope = serde_json::to_value(report::Envelope::of("resume", done)).unwrap();
            assert!(validator.is_valid(&envelope), "{envelope:#}");
        }
    }

    #[test]
    fn a_daemon_waits_asynchronously_for_a_stopped_profile_reader() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let held = Driver::open(paths.clone()).unwrap();
            let release = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                drop(held);
            });
            let opened =
                tokio::time::timeout(std::time::Duration::from_secs(2), daemon_driver(paths))
                    .await
                    .expect("the daemon must yield while a status reader holds the profile");
            assert!(
                opened.is_ok(),
                "the daemon must start after the reader releases the profile"
            );
            release.await.unwrap();
        });
    }

    #[tokio::test]
    async fn a_daemon_does_not_wait_for_another_daemon_that_already_listens() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let _held = Driver::open(paths.clone()).unwrap();
        let _listener = koidrive_ipc::bind(&paths.socket()).unwrap();
        let opened = tokio::time::timeout(std::time::Duration::from_secs(1), daemon_driver(paths))
            .await
            .unwrap();
        assert!(matches!(opened, Err(DriverError::Busy(_))));
    }

    #[test]
    fn setup_uses_the_final_service_handoff_without_changing_reconcile_results() {
        for (initial_state, later_state, app_failed) in [
            ("unavailable", "running", false),
            ("running", "unavailable", true),
        ] {
            for setup_app in [false, true] {
                for has_handoff in [false, true] {
                    let report = |state| -> service::Report {
                        serde_json::from_value(serde_json::json!({
                            "manager":"launchd", "name":"fixture", "state":state,
                            "problem": if state == "unavailable" { Some("fixture service failure") } else { None }
                        })).unwrap()
                    };
                    let initial = report(initial_state);
                    let later = report(later_state);
                    let selected = if setup_app && has_handoff {
                        &later
                    } else {
                        &initial
                    };
                    let expected_service = serde_json::to_value(selected).unwrap();
                    let expected_text = match selected.text() {
                        text if text.is_empty() && !setup_app => selected.status_line(),
                        text => text,
                    };
                    let app: app::Report = serde_json::from_value(serde_json::json!({
                        "state":"present", "path":"/fixture/Koidrive.app", "version":"99.0.0",
                        "change":"installed", "launch":"not_requested",
                        "problem": app_failed.then_some("fixture app failure")
                    }))
                    .unwrap();
                    let expected_app = serde_json::to_value(&app).unwrap();
                    let app_text = app.text();
                    let result = finish_lifecycle(
                        Done::ok(serde_json::json!({"device":"fixture"}), "Connected\n"),
                        Some(initial),
                        Some((app, has_handoff.then_some(later))),
                        setup_app,
                        !setup_app,
                    );
                    let done = if setup_app && app_failed {
                        let error = result.unwrap_err();
                        assert_eq!(error.code, Code::Failed);
                        *error.partial.unwrap()
                    } else {
                        result.unwrap()
                    };
                    assert_eq!(done.data["service"], expected_service);
                    assert_eq!(done.data["app"], expected_app);
                    assert_eq!(done.data["device"], "fixture");
                    assert_eq!(done.text, format!("Connected\n{expected_text}{app_text}"));
                }
            }
        }
    }

    #[tokio::test]
    async fn setup_finish_bounds_a_status_request_that_accepts_but_never_replies() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
        let (accepted, received) = tokio::sync::oneshot::channel();
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = koidrive_ipc::read_line(&mut stream).await.unwrap();
            assert!(matches!(
                serde_json::from_str::<Request>(&request).unwrap(),
                Request::Status
            ));
            accepted.send(()).unwrap();
            std::future::pending::<()>().await;
            drop(stream);
        });
        let report: service::Report = serde_json::from_value(serde_json::json!({
            "manager":"systemd", "name":"fixture", "state":"running", "lingers":true
        }))
        .unwrap();
        let done = Done {
            note: "Setup complete. Add files to ~/Koidrive.\nCheck sync: koi status\n".into(),
            ..Done::printed()
        };
        let terminal = terminal::Terminal::setup(true, true);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            finish_setup(&paths, done, Some(report), None, &terminal),
        )
        .await;
        served.abort();
        let _ = served.await;
        received
            .await
            .expect("the daemon must have received the final status request");
        let done = result
            .expect("setup must stop waiting for a silent status response")
            .unwrap();
        assert_eq!(
            done.note,
            "Folder linked. Could not confirm background sync.\nCheck sync: koi status\n"
        );
        assert!(!done.note.contains("Setup complete"));
        assert!(!done.note.contains("Background sync is running"));
        assert_eq!(done.data["service"]["state"], "running");
    }

    #[tokio::test]
    async fn setup_finish_reports_pending_decisions_in_active_folders() {
        const FIXTURE: &str = "KOIDRIVE_TEST_SETUP_PENDING";
        let Ok(case) = std::env::var(FIXTURE) else {
            for case in ["connecting", "bootstrapping", "catching_up", "up_to_date"] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "tests::setup_finish_reports_pending_decisions_in_active_folders",
                        "--nocapture",
                    ])
                    .env(FIXTURE, case)
                    .output()
                    .unwrap();
                let text = String::from_utf8(output.stderr).unwrap();
                assert!(
                    output.status.success(),
                    "{case}: {text}\n{}",
                    String::from_utf8_lossy(&output.stdout)
                );
                assert!(
                    text.contains("Review pending decisions for /fixture/Koidrive: koi resolve"),
                    "{case}: {text}"
                );
                assert!(
                    !text.contains("Background sync is running"),
                    "{case}: {text}"
                );
                assert!(!text.contains("Sync is offline"), "{case}: {text}");
            }
            return;
        };
        let state = match case.as_str() {
            "connecting" => FolderState::Connecting,
            "bootstrapping" => FolderState::Bootstrapping,
            "catching_up" => FolderState::CatchingUp,
            "up_to_date" => FolderState::UpToDate,
            _ => panic!("unknown fixture {case}"),
        };
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = koidrive_ipc::read_line(&mut stream).await.unwrap();
            assert!(matches!(
                serde_json::from_str::<Request>(&request).unwrap(),
                Request::Status
            ));
            let status = Status {
                running: true,
                folders: vec![
                    serde_json::from_value(serde_json::json!({
                        "space_id": "s1",
                        "space_name": "Koidrive",
                        "path": "/fixture/Koidrive",
                        "state": state,
                        "history_id": null,
                        "last_seq": null,
                        "head_seq": null,
                        "remote_nodes": 0,
                        "local_entries": null,
                        "questions": 1,
                        "problem": null,
                        "wait": null
                    }))
                    .unwrap(),
                ],
                ..Status::default()
            };
            koidrive_ipc::write_line(&mut stream, &Response::Status { status })
                .await
                .unwrap();
        });
        let report: service::Report = serde_json::from_value(serde_json::json!({
            "manager":"systemd", "name":"fixture", "state":"running", "lingers":true
        }))
        .unwrap();
        let done = finish_setup(
            &paths,
            Done {
                note: "Setup complete. Add files to ~/Koidrive.\nCheck sync: koi status\n".into(),
                ..Done::printed()
            },
            Some(report),
            None,
            &terminal::Terminal::setup(true, true),
        )
        .await
        .unwrap();
        served.await.unwrap();
        assert_eq!(
            done.note,
            "Folder linked. Sync needs attention.\nCheck sync: koi status\n"
        );
    }

    #[tokio::test]
    async fn setup_finish_preserves_the_linux_logout_warning() {
        const FIXTURE: &str = "KOIDRIVE_TEST_SETUP_LINGER";
        let Ok(case) = std::env::var(FIXTURE) else {
            for (case, warns) in [
                ("systemd-off", true),
                ("systemd-on", false),
                ("systemd-unknown", false),
                ("launchd-off", false),
            ] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "tests::setup_finish_preserves_the_linux_logout_warning",
                        "--nocapture",
                    ])
                    .env(FIXTURE, case)
                    .output()
                    .unwrap();
                let text = String::from_utf8(output.stderr).unwrap();
                assert!(
                    output.status.success(),
                    "{case}: {text}\n{}",
                    String::from_utf8_lossy(&output.stdout)
                );
                assert_eq!(
                    text.contains("stops when you log out"),
                    warns,
                    "{case}: {text}"
                );
                assert_eq!(
                    text.contains("sudo loginctl enable-linger \"$USER\""),
                    warns,
                    "{case}: {text}"
                );
            }
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = koidrive_ipc::read_line(&mut stream).await.unwrap();
            assert!(matches!(
                serde_json::from_str::<Request>(&request).unwrap(),
                Request::Status
            ));
            let status = Status {
                running: true,
                folders: vec![
                    serde_json::from_value(serde_json::json!({
                        "space_id": "s1",
                        "space_name": "Koidrive",
                        "path": "/fixture/Koidrive",
                        "state": FolderState::UpToDate,
                        "history_id": null,
                        "last_seq": null,
                        "head_seq": null,
                        "remote_nodes": 0,
                        "local_entries": null,
                        "questions": 0,
                        "problem": null,
                        "wait": null
                    }))
                    .unwrap(),
                ],
                ..Status::default()
            };
            koidrive_ipc::write_line(&mut stream, &Response::Status { status })
                .await
                .unwrap();
        });
        let (manager, lingers) = match case.as_str() {
            "systemd-off" => ("systemd", Some(false)),
            "systemd-on" => ("systemd", Some(true)),
            "systemd-unknown" => ("systemd", None),
            "launchd-off" => ("launchd", Some(false)),
            _ => panic!("unknown fixture {case}"),
        };
        let report: service::Report = serde_json::from_value(serde_json::json!({
            "manager":manager, "name":"fixture", "state":"running", "lingers":lingers
        }))
        .unwrap();
        let note = "Setup complete. Add files to ~/Koidrive.\nCheck sync: koi status\n";
        let done = finish_setup(
            &paths,
            Done {
                note: note.into(),
                ..Done::printed()
            },
            Some(report),
            None,
            &terminal::Terminal::setup(true, true),
        )
        .await
        .unwrap();
        served.await.unwrap();
        assert_eq!(done.note, note);
        assert_eq!(done.data["service"]["manager"], manager);
    }

    #[test]
    fn every_command_parses_in_its_group() {
        let parses = [
            &["koi", "status", "--check"][..],
            &["koi", "resolve"],
            &["koi", "setup", "--interactive"],
            &["koi", "join", "K7QM-2XPD@files.example.com"],
            &["koi", "server", "deploy"],
            &["koi", "server", "add-device", "--worker", "koidrive"],
            &[
                "koi",
                "server",
                "claim",
                "files.example.com",
                "ABCD",
                "--user",
                "Ada",
            ],
            &["koi", "server", "upgrade", "--worker", "koidrive", "--yes"],
            &["koi", "server", "delete", "--worker", "koidrive", "--yes"],
            &[
                "koi",
                "server",
                "delete",
                "--bucket",
                "koidrive-1a2b",
                "--yes",
            ],
            &["koi", "space", "create", "Notes"],
            &["koi", "space", "list"],
            &["koi", "space", "usage"],
            &["koi", "folder", "link", "dir", "--space", "s"],
            &["koi", "folder", "unlink", "dir"],
            &["koi", "folder", "restart", "dir"],
            &["koi", "folder", "retry", "dir", "--resend"],
            &["koi", "folder", "retry", "dir", "--replan"],
            &["koi", "device", "invite"],
            &["koi", "device", "list"],
            &["koi", "device", "revoke", ID],
            &["koi", "device", "leave"],
            &["koi", "device", "leave", "--last-device"],
            &["koi", "history", "trash"],
            &["koi", "history", "versions", "a.txt"],
            &["koi", "history", "restore", "a.txt", "--node", "n1"],
            &["koi", "history", "requests"],
            &["koi", "_daemon", "--log-format", "json"],
        ];
        for args in parses {
            assert!(
                parse(args).is_ok(),
                "{args:?}: {}",
                parse(args).err().unwrap()
            );
        }
        assert!(matches!(
            parse(&["koi", "history", "trash"]),
            Ok(Some(Command::History {
                action: history::HistoryAction::Trash { folder }
            })) if folder == Path::new(".")
        ));
        assert!(matches!(
            parse(&["koi", "history", "requests"]),
            Ok(Some(Command::History {
                action: history::HistoryAction::Requests { folder }
            })) if folder == Path::new(".")
        ));
        assert!(parse(&["koi", "folder", "retry", "dir"]).is_err());
        assert!(parse(&["koi", "folder", "retry", "dir", "--resend", "--replan"]).is_err());
        assert!(parse(&["koi", "folder", "link", "--space", "s"]).is_err());
        assert!(parse(&["koi", "history", "versions"]).is_err());
    }

    #[test]
    fn the_old_command_names_are_gone_without_aliases() {
        let old = [
            &["koi", "setup", "K7QM-2XPD@files.example.com"][..],
            &["koi", "link", "dir", "--space", "s"],
            &["koi", "unlink", "dir"],
            &["koi", "restart", "dir"],
            &["koi", "lift", "dir", "--resend"],
            &["koi", "folder", "lift", "dir", "--resend"],
            &["koi", "questions"],
            &["koi", "trash"],
            &["koi", "versions", "a.txt"],
            &["koi", "restore", "a.txt"],
            &["koi", "restores"],
            &["koi", "history", "restores"],
            &["koi", "usage"],
            &["koi", "daemon"],
            &["koi", "device", "add"],
            &["koi", "server", "uninstall"],
            &["koi", "server", "upgrade", "--server", "koidrive"],
            &["koi", "server", "add-device", "--server", "koidrive"],
            &["koi", "status", "--json=2"],
        ];
        for args in old {
            assert!(parse(args).is_err(), "{args:?} still parses");
        }
        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("_daemon"), "{help}");
        for group in [
            "status", "resolve", "setup", "join", "server", "space", "folder", "device", "history",
        ] {
            assert!(
                help.contains(&format!("  {group} ")),
                "{group} missing from {help}"
            );
        }
        assert!(help.contains("Koidrive sync client"), "{help}");
    }

    #[test]
    fn json_is_one_global_flag_for_schema_1() {
        for args in [
            &["koi", "--json", "status"][..],
            &["koi", "status", "--json"],
            &["koi", "status", "--json=1"],
            &["koi", "folder", "link", "dir", "--json", "--space", "s"],
            &["koi", "history", "versions", "a.txt", "--json"],
        ] {
            assert_eq!(Cli::try_parse_from(args).unwrap().json, Some(1), "{args:?}");
        }
        assert_eq!(Cli::try_parse_from(["koi", "status"]).unwrap().json, None);
        assert!(Cli::try_parse_from(["koi", "status", "--json=0"]).is_err());
    }

    #[test]
    fn a_group_without_its_subcommand_shows_help_and_does_nothing() {
        for group in ["server", "space", "folder", "device", "history"] {
            let error = Cli::try_parse_from(["koi", group]).err().expect("help");
            assert_eq!(
                error.kind(),
                ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
                "{group}"
            );
        }
        assert!(matches!(parse(&["koi"]), Ok(None)));
    }

    #[test]
    fn the_command_path_and_json_are_read_from_the_raw_arguments() {
        let args = |words: &[&str]| -> Vec<OsString> { words.iter().map(OsString::from).collect() };
        assert_eq!(
            command_path(&args(&[
                "koi", "--json", "folder", "link", "dir", "--space", "s"
            ])),
            "folder link"
        );
        assert_eq!(command_path(&args(&["koi", "status", "--check"])), "status");
        assert_eq!(command_path(&args(&["koi", "history"])), "history");
        assert_eq!(command_path(&args(&["koi", "nonsense", "x"])), "");
        assert_eq!(command_path(&args(&["koi"])), "");
        assert_eq!(command_path(&args(&["koi", "-V", "space", "list"])), "");
        assert_eq!(
            command_path(&args(&["koi", "space", "--help", "list"])),
            "space"
        );
        assert_eq!(command_path(&args(&["koi", "--", "status"])), "");
        assert_eq!(
            command_path(&args(&["koi", "space", "--bogus", "help"])),
            "space"
        );
        assert_eq!(command_path(&args(&["koi", "-Vh", "space", "list"])), "");
        assert_eq!(command_path(&args(&["koi", "-hV", "folder", "link"])), "");
        assert_eq!(
            command_path(&args(&["koi", "space", "-V", "list"])),
            "space list"
        );
        assert!(json_requested(&args(&["koi", "linky", "--json"])));
        assert!(json_requested(&args(&["koi", "status", "--json=2"])));
        assert!(!json_requested(&args(&["koi", "status"])));
        assert!(!json_requested(&args(&[
            "koi", "history", "versions", "--", "--json"
        ])));
    }

    #[tokio::test]
    async fn a_reload_refuses_a_credential_for_another_server() {
        use crate::setup::fake::{Fake, Reply};
        use prost::Message;

        let refused = || Reply {
            method: "POST",
            path: "/v1/token".into(),
            status: 401,
            content_type: "application/x-protobuf",
            body: koidrive_proto::v1::Error {
                code: koidrive_proto::v1::error::Code::Unauthenticated as i32,
                message: "unknown device or refresh credential".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        };
        let first_server = Fake::start(vec![refused(), refused(), refused()]).await;
        let other_server = Fake::start(Vec::new()).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let first = Credential {
            server: first_server.base.to_string(),
            user_id: "same-user".into(),
            user_name: None,
            device_id: ID.into(),
            device_name: "same-device".into(),
            refresh_credential: "first-credential".into(),
        };
        first.save_new(&paths.credential()).unwrap();
        let reader = reading(&paths, &first).unwrap();
        let owner = Driver::open(paths.clone()).unwrap();
        owner.sign_out_device(ID).await.unwrap();
        let replacement = Credential {
            server: other_server.base.to_string(),
            refresh_credential: "other-server-credential".into(),
            ..first
        };
        owner.save_credential(replacement.clone()).await.unwrap();

        assert!(matches!(
            reader.access_token().await,
            Err(ApiError::Credential(_))
        ));
        let presented: Vec<String> = first_server
            .seen()
            .iter()
            .map(|seen| {
                koidrive_proto::v1::TokenRequest::decode(seen.body.as_slice())
                    .unwrap()
                    .refresh_credential
            })
            .collect();
        assert!(
            !presented.contains(&replacement.refresh_credential),
            "{presented:?}"
        );
    }

    #[tokio::test]
    async fn a_reload_never_presents_another_profiles_credential_to_the_first_server() {
        use crate::setup::fake::{Fake, Reply};
        use prost::Message;

        let refused = || Reply {
            method: "POST",
            path: "/v1/token".into(),
            status: 401,
            content_type: "application/x-protobuf",
            body: koidrive_proto::v1::Error {
                code: koidrive_proto::v1::error::Code::Unauthenticated as i32,
                message: "unknown device or refresh credential".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        };
        let first_server = Fake::start(vec![refused(), refused(), refused()]).await;
        let other_server = Fake::start(Vec::new()).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let first = Credential {
            server: first_server.base.to_string(),
            user_id: "first-user".into(),
            user_name: None,
            device_id: ID.into(),
            device_name: "first".into(),
            refresh_credential: "first-credential".into(),
        };
        first.save_new(&paths.credential()).unwrap();
        let reader = reading(&paths, &first).unwrap();
        let owner = Driver::open(paths.clone()).unwrap();
        owner.sign_out_device(ID).await.unwrap();
        let replacement = Credential {
            server: other_server.base.to_string(),
            user_id: "other-user".into(),
            user_name: None,
            device_id: "0190a0b0-0000-7000-8000-000000000002".into(),
            device_name: "other".into(),
            refresh_credential: "other-credential".into(),
        };
        owner.save_credential(replacement.clone()).await.unwrap();

        assert!(matches!(
            reader.access_token().await,
            Err(ApiError::Credential(_))
        ));
        let presented: Vec<String> = first_server
            .seen()
            .iter()
            .map(|seen| {
                koidrive_proto::v1::TokenRequest::decode(seen.body.as_slice())
                    .unwrap()
                    .refresh_credential
            })
            .collect();
        assert!(
            !presented.contains(&replacement.refresh_credential),
            "{presented:?}"
        );
    }

    fn held(
        (deletions, affected_files, space_files): (u64, u64, u64),
        exceeded: &[Limit],
        items: usize,
    ) -> Question {
        Question {
            id: "q1".into(),
            lineage: "q1".into(),
            space_id: "s1".into(),
            asked_at_ms: 1,
            recovery_cause: None,
            kind: QuestionKind::MassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded: exceeded.to_vec(),
            },
            items: (0..items)
                .map(|i| koidrive_driver::HeldItem {
                    node_id: format!("n{i}"),
                    path: format!("old/{i:02}"),
                    files: 1,
                    device_id: None,
                })
                .collect(),
        }
    }

    const ORIGINAL: &str = "0190a0b0-0000-7000-8000-00000000000a";
    const COPY: &str = "0190a0b0-0000-7000-8000-00000000000b";
    const DESK: &str = "0190a0b0-0000-7000-8000-0000000000d1";
    const LAPTOP: &str = "0190a0b0-0000-7000-8000-0000000000d2";

    /// A conflict between docs/notes.txt, written by DESK, and its conflict copy written by LAPTOP, with the items in
    /// path order as the driver lists them.
    fn conflict(copy_path: &str) -> Question {
        use koidrive_engine::question::{ConflictFile, Revisions};

        let file = |node_id: &str| ConflictFile {
            node_id: node_id.into(),
            revisions: Revisions {
                content_rev: 2,
                meta_rev: 1,
            },
        };
        let item = |node_id: &str, path: &str, device_id: &str| koidrive_driver::HeldItem {
            node_id: node_id.into(),
            path: path.into(),
            files: 1,
            device_id: Some(device_id.into()),
        };
        Question {
            kind: QuestionKind::Conflict {
                original: file(ORIGINAL),
                copy: file(COPY),
                sent_in: None,
            },
            items: vec![
                item(COPY, copy_path, LAPTOP),
                item(ORIGINAL, "docs/notes.txt", DESK),
            ],
            ..held((0, 0, 0), &[], 0)
        }
    }

    #[test]
    fn a_conflict_answer_keeps_the_node_it_names_or_both() {
        let question = conflict("docs/notes (conflict).txt");
        assert_eq!(
            answer_to(&question, &format!("keep-node:{ORIGINAL}")),
            Ok(Answer::KeepOriginal)
        );
        assert_eq!(
            answer_to(&question, &format!("keep-node:{COPY}")),
            Ok(Answer::KeepCopy)
        );
        assert_eq!(answer_to(&question, "keep-both"), Ok(Answer::KeepBoth));
        let expected = format!(
            "is not an answer to q1; use keep-node:{ORIGINAL} or keep-node:{COPY} or keep-both"
        );
        for word in [
            format!("keep-node:{DESK}"),
            "restore".to_owned(),
            "keep_both".to_owned(),
        ] {
            let error = answer_to(&question, &word).unwrap_err();
            assert_eq!(error, format!("{word} {expected}"));
        }
    }

    #[test]
    fn a_decision_other_than_a_conflict_takes_the_answers_it_did() {
        let question = held((30, 30, 40), &[Limit::Share], 1);
        assert_eq!(answer_to(&question, "delete"), Ok(Answer::Delete));
        assert_eq!(answer_to(&question, "restore"), Ok(Answer::Restore));
        assert_eq!(
            answer_to(&question, "keep-both"),
            Err("keep-both is not an answer to q1; use delete or restore".to_owned())
        );
        assert_eq!(
            answer_to(&question, &format!("keep-node:{ORIGINAL}")),
            Err(format!(
                "keep-node:{ORIGINAL} is not an answer to q1; use delete or restore"
            ))
        );
    }

    #[test]
    fn resolve_answers_a_conflict_only_with_its_own_words_and_names_them_otherwise() {
        let parse = |word: &str| match Cli::try_parse_from(["koi", "resolve", "q1", word]) {
            Ok(Cli {
                command:
                    Some(Command::Resolve {
                        answer: Some(answer),
                        ..
                    }),
                ..
            }) => answer,
            _ => panic!("resolve q1 {word} parses as an answer"),
        };
        let question = conflict("docs/notes (conflict).txt");
        assert_eq!(
            answer_to(&question, &parse(&format!("keep-node:{COPY}"))),
            Ok(Answer::KeepCopy)
        );
        assert_eq!(
            answer_to(&question, &parse("keep-both")),
            Ok(Answer::KeepBoth)
        );
        let valid = format!("use keep-node:{ORIGINAL} or keep-node:{COPY} or keep-both");
        for word in ["nonsense", "keep_original", "keep-node:", "delete"] {
            let error = answer_to(&question, &parse(word)).unwrap_err();
            assert_eq!(error, format!("{word} is not an answer to q1; {valid}"));
        }
    }

    #[test]
    fn resolve_lists_both_files_of_a_conflict_with_their_writers_and_one_command_per_answer() {
        assert_eq!(
            question_text(&conflict("docs/notes (conflict).txt")),
            format!(
                "q1  space s1  docs/notes.txt has a conflict copy\n\
                 \x20   Original: docs/notes.txt, written by device {DESK}\n\
                 \x20   Conflict copy: 'docs/notes (conflict).txt', written by device {LAPTOP}\n\
                 \x20 Keep docs/notes.txt and move 'docs/notes (conflict).txt' to the trash:\n\
                 \x20   koi resolve q1 keep-node:{ORIGINAL}\n\
                 \x20 Give docs/notes.txt the content of 'docs/notes (conflict).txt', keeping the old content in its \
                 version history, and move 'docs/notes (conflict).txt' to the trash:\n\
                 \x20   koi resolve q1 keep-node:{COPY}\n\
                 \x20 Keep both files:\n\
                 \x20   koi resolve q1 keep-both\n"
            )
        );
    }

    #[test]
    fn status_names_a_newer_release_after_the_service_line_only_while_one_is_out() {
        use koidrive_driver::Update;

        let service = "Background service: running (koidrive.service)\n";
        let status = |update| Status {
            running: true,
            version: Some("0.1.0".into()),
            update,
            ..Status::default()
        };
        let available = status(Update::Available {
            latest: "0.1.2".into(),
            url: "https://github.com/koidrive/koidrive/releases/tag/v0.1.2".into(),
            checked_at_ms: 1,
        });
        let notice = |status: &Status, command| {
            UpdateNoticeData::of(
                &status.update,
                status
                    .version
                    .as_deref()
                    .unwrap_or(env!("CARGO_PKG_VERSION")),
                command,
            )
        };
        let text = status_text(
            &available,
            &[],
            service,
            &notice(&available, Some("koi update")),
        );
        assert!(
            text.contains(&format!(
                "{service}Update: Koidrive 0.1.2 is out (this device runs 0.1.0). Run: koi update\n"
            )),
            "{text}"
        );
        for quiet in [
            Update::UpToDate { checked_at_ms: 1 },
            Update::Unknown,
            Update::Off,
        ] {
            let quiet = status(quiet);
            let text = status_text(&quiet, &[], service, &notice(&quiet, Some("koi update")));
            assert!(!text.contains("Update:"), "{text}");
        }
        let unversioned = Status {
            version: None,
            ..available.clone()
        };
        assert!(
            status_text(
                &unversioned,
                &[],
                service,
                &notice(&unversioned, Some("koi update"))
            )
            .contains(concat!(
                "(this device runs ",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
        );
        for command in ["brew upgrade --cask koidrive", "brew upgrade koidrive"] {
            let text = status_text(&available, &[], service, &notice(&available, Some(command)));
            assert!(text.contains(&format!("Run: {command}\n")), "{text}");
            assert!(!text.contains("koi update"), "{text}");
        }
        let text = status_text(&available, &[], service, &notice(&available, None));
        assert!(
            text.contains("Download: https://github.com/koidrive/koidrive/releases/tag/v0.1.2"),
            "{text}"
        );
        assert!(!text.contains("Run:"), "{text}");
    }

    #[test]
    fn status_shows_a_conflict_under_its_folder_and_a_notification_names_its_file() {
        let question = conflict("docs/notes (conflict).txt");
        let brief = question_brief(&question);
        let lines: Vec<&str> = brief.lines().collect();
        assert_eq!(
            lines[..3],
            [
                "  q1: docs/notes.txt has a conflict copy",
                &format!("    Original: docs/notes.txt, written by device {DESK}"),
                &format!(
                    "    Conflict copy: 'docs/notes (conflict).txt', written by device {LAPTOP}"
                ),
            ]
        );
        assert_eq!(
            lines[3..]
                .iter()
                .filter(|line| line.starts_with("    koi resolve q1 "))
                .count(),
            3
        );
        assert_eq!(lines.len(), 9);
        assert_eq!(
            question_notice(&question),
            "docs/notes.txt has a conflict copy. Run koi resolve."
        );
    }

    #[test]
    fn a_conflict_path_is_quoted_with_its_control_characters_escaped() {
        let question = conflict("docs/it's\nforged\u{1b}[2J");
        for text in [question_text(&question), question_brief(&question)] {
            assert_eq!(text.lines().count(), 9, "{text}");
            assert!(!text.contains('\u{1b}'), "{text}");
            assert!(
                text.contains(r"Conflict copy: 'docs/it'\''s\u{a}forged\u{1b}[2J', written by"),
                "{text}"
            );
        }
        let mut unwritten = conflict("docs/notes (conflict).txt");
        unwritten.items[0].device_id = None;
        assert!(question_text(&unwritten).contains(
            "Conflict copy: 'docs/notes (conflict).txt', written by an unknown device\n"
        ));
    }

    #[test]
    fn a_held_path_is_shown_with_its_control_characters_escaped() {
        let mut question = held((1, 15, 20), &[Limit::Share], 1);
        question.items[0].path = "old/a\nforged line\u{1b}[2J".into();
        for text in [question_text(&question), question_brief(&question)] {
            assert_eq!(
                text.lines().count(),
                6 - text.starts_with(' ') as usize,
                "{text}"
            );
            assert!(!text.contains('\u{1b}'), "{text}");
        }
    }

    #[test]
    fn a_mass_delete_question_names_the_limit_it_crossed_and_lists_at_most_20_items() {
        let text = question_text(&held((1_001, 1_001, 5_000), &[Limit::Deletions], 25));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[..6],
            [
                "q1  space s1  held 1001 deletions that would remove 1001 of 5000 files, more than 1000 deletions in 10 minutes",
                "  Delete these files on the server and other devices:",
                "    koi resolve q1 delete",
                "  Download these files to this device again:",
                "    koi resolve q1 restore",
                "  old/00 (1 file)",
            ]
        );
        assert_eq!(lines.len(), 5 + ITEMS_SHOWN + 1);
        assert_eq!(lines[24], "  old/19 (1 file)");
        assert_eq!(lines[25], "  and 5 more; koi resolve --json lists them all");

        let text = question_text(&held((1, 15, 20), &[Limit::Share], 1));
        assert_eq!(
            text.lines().next(),
            Some(
                "q1  space s1  held 1 deletions that would remove 15 of 20 files, more than half of the space in 10 minutes"
            )
        );
        assert_eq!(text.lines().count(), 6);
    }

    #[test]
    fn status_shows_a_decision_under_its_folder_with_its_first_path_and_one_full_command_per_answer()
     {
        assert_eq!(
            question_brief(&held((30, 30, 40), &[Limit::Share], 3)),
            [
                "  q1: held 30 deletions that would remove 30 of 40 files, more than half of the space in 10 minutes; first path: old/00",
                "  Delete these files on the server and other devices:",
                "    koi resolve q1 delete",
                "  Download these files to this device again:",
                "    koi resolve q1 restore",
                "",
            ]
            .join("\n")
        );
        assert_eq!(
            question_brief(&held((30, 30, 40), &[Limit::Share], 0))
                .lines()
                .next(),
            Some(
                "  q1: held 30 deletions that would remove 30 of 40 files, more than half of the space in 10 minutes"
            )
        );
    }

    #[test]
    fn json_output_and_the_commands_that_show_decisions_get_no_banner() {
        let shows = |args: &[&str]| Cli::try_parse_from(args).unwrap().shows_banner();
        assert!(shows(&["koi", "history", "trash", "."]));
        assert!(shows(&["koi", "device", "list"]));
        assert!(shows(&["koi", "folder", "link", "dir", "--space", "s"]));
        for args in [
            &["koi", "history", "trash", ".", "--json"][..],
            &["koi", "device", "list", "--json"],
            &["koi", "space", "list", "--json"],
            &["koi", "space", "usage", "--json"],
            &["koi", "status"],
            &["koi", "resolve"],
            &["koi", "resolve", "q1", "delete"],
            &["koi", "_daemon"],
        ] {
            assert!(!shows(args), "{args:?}");
        }
        let reports = |args: &[&str]| {
            (Cli::try_parse_from(args).unwrap().command)
                .is_some_and(|command| command.reports_pending())
        };
        assert!(reports(&["koi", "resolve", "q1", "delete", "--json"]));
        assert!(!reports(&["koi", "resolve", "--json"]));
        assert!(!reports(&["koi", "status", "--json"]));
    }

    #[test]
    fn the_banner_counts_the_pending_decisions_and_names_resolve() {
        assert_eq!(banner(&[]), None);
        assert_eq!(
            banner(&[held((30, 30, 40), &[Limit::Share], 1)]).as_deref(),
            Some("koi: 1 needs you; run koi resolve")
        );
        assert_eq!(
            banner(&[
                held((30, 30, 40), &[Limit::Share], 1),
                held((2_000, 2_000, 9_000), &[Limit::Deletions], 1)
            ])
            .as_deref(),
            Some("koi: 2 need you; run koi resolve")
        );
    }

    #[test]
    fn a_notification_names_the_decision_its_first_path_and_resolve() {
        assert_eq!(
            question_notice(&held((30, 30, 40), &[Limit::Share], 2)),
            "held 30 deletions that would remove 30 of 40 files, more than half of the space in 10 minutes; first path: old/00. Run koi resolve."
        );
    }

    #[test]
    fn resolve_lists_with_no_arguments_and_answers_with_an_id_and_an_answer() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|cli| cli.command);
        assert!(matches!(
            parse(&["koi", "resolve"]),
            Ok(Some(Command::Resolve {
                id: None,
                answer: None,
            }))
        ));
        assert!(matches!(
            parse(&["koi", "resolve", "q1", "restore"]),
            Ok(Some(Command::Resolve {
                id: Some(_),
                answer: Some(answer),
                ..
            })) if answer == "restore"
        ));
        assert!(parse(&["koi", "resolve", "q1"]).is_err());
        assert_eq!(
            answer_to(&held((30, 30, 40), &[Limit::Share], 1), "later"),
            Err("later is not an answer to q1; use delete or restore".to_owned())
        );
        assert!(matches!(
            parse(&["koi", "resolve", "q1", "delete", "--json"]),
            Ok(Some(Command::Resolve { id: Some(_), .. }))
        ));
        assert!(parse(&["koi", "questions"]).is_err());
    }

    #[tokio::test]
    async fn a_build_without_the_server_bundle_stops_before_the_cloudflare_login() {
        if option_env!("KOIDRIVE_SERVER_BUNDLE").is_some() {
            return;
        }
        for args in [
            vec!["koi", "server", "deploy"],
            vec!["koi", "server", "add-device"],
            vec!["koi", "server", "upgrade"],
            vec!["koi", "server", "delete"],
        ] {
            let Some(Command::Server { action }) = Cli::try_parse_from(&args).unwrap().command
            else {
                unreachable!("{args:?} is a server command");
            };
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let outcome = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                setup::server(
                    &paths,
                    action,
                    false,
                    Arc::new(terminal::Terminal::new(true, false)),
                ),
            )
            .await
            .unwrap_or_else(|_| panic!("{args:?} waited for a login"));
            let error = outcome.unwrap_err();
            assert_eq!(error.code, Code::AuthRequired, "{args:?}: {error:?}");
        }
        let Err(missing) = setup::embedded_bundle() else {
            panic!("a build without the server bundle unpacked one");
        };
        assert!(
            missing.message.contains("built without a server bundle"),
            "{missing:?}"
        );
    }

    #[test]
    fn a_question_names_every_limit_crossed_and_a_notice_takes_dismiss() {
        let both = [Limit::Deletions, Limit::Share];
        let text = question_text(&held((1_001, 3_000, 5_000), &both, 1));
        assert_eq!(
            text.lines().next(),
            Some(
                "q1  space s1  held 1001 deletions that would remove 3000 of 5000 files, more than 1000 deletions in 10 minutes and more than half of the space in 10 minutes"
            )
        );

        let notice = Question {
            kind: QuestionKind::RemoteMassDelete {
                deletions: 1,
                affected_files: 15,
                space_files: 20,
                exceeded: vec![Limit::Share],
            },
            ..held((1, 15, 20), &[Limit::Share], 0)
        };
        let text = question_text(&notice);
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "q1  space s1  applied 1 deletions from other devices that removed 15 of 20 files, more than half of the space in 10 minutes",
                "  Close this notice:",
                "    koi resolve q1 dismiss",
            ]
        );
    }

    #[test]
    fn a_first_link_question_reads_as_a_sentence_with_its_answers() {
        let mut question = held((0, 0, 0), &[], 1);
        question.kind = QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
            sent_in: None,
            cause: None,
        };
        let text = question_text(&question);
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "q1  space s1  files on this device that the space does not hold",
                "  Send this device's version to the server:",
                "    koi resolve q1 publish",
                "  Move this device's copies to the local trash and follow the server:",
                "    koi resolve q1 discard",
                "  old/00 (1 file)",
            ]
        );
    }

    #[test]
    fn a_name_longer_than_255_bytes_is_refused_before_anything_is_sent() {
        let longest = "가".repeat(85);
        let too_long = format!("{longest}a");
        for args in [
            vec!["koi", "space", "create", &too_long],
            vec![
                "koi",
                "join",
                "K7QM-2XPD@files.example.com",
                "--name",
                &too_long,
            ],
            vec![
                "koi",
                "server",
                "claim",
                "files.example.com",
                "ABCD",
                "--user",
                &too_long,
            ],
        ] {
            let error = Cli::try_parse_from(&args)
                .err()
                .expect("a 256-byte name is refused");
            assert!(
                error
                    .to_string()
                    .contains("a name is at most 255 bytes of UTF-8, and this one has 256"),
                "{error}"
            );
        }
        assert!(Cli::try_parse_from(["koi", "space", "create", &longest]).is_ok());
    }

    #[test]
    fn a_device_id_is_a_uuid_and_nothing_that_could_name_another_path() {
        assert_eq!(device_id(ID).as_deref(), Ok(ID));
        assert_eq!(device_id(&ID.to_uppercase()).as_deref(), Ok(ID));
        for text in [
            "",
            "../spaces",
            "0190a0b0-0000-7000-8000-00000000000",
            &format!("{ID}/x"),
            "0190a0b0x0000-7000-8000-000000000001",
        ] {
            assert!(device_id(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn names_and_platforms_cannot_send_terminal_controls_or_forge_rows() {
        let forged = DeviceInfo {
            device_id: ID.into(),
            name: "\u{1b}[2J\u{1b}[Hsafe-looking\u{202e}".into(),
            platform: "linux\nFORGED ROW\u{9b}".into(),
            ..Default::default()
        };
        let table = device_table(&[forged], 0);
        assert_eq!(table.lines().count(), 2, "{table}");
        assert!(
            !table.chars().any(|c| c.is_control() && c != '\n'),
            "{table:?}"
        );
        assert!(
            table.contains(r"\u{1b}[2J\u{1b}[Hsafe-looking\u{202e}  linux\u{a}FORGED ROW\u{9b}"),
            "{table}"
        );
        assert_eq!(
            serde_json::to_value(DeviceData::of(&DeviceInfo {
                name: "a\nb".into(),
                ..Default::default()
            }))
            .unwrap()["name"],
            "a\nb"
        );
    }

    #[test]
    fn times_read_as_how_long_ago() {
        let now = 1_000 * 86_400_000;
        assert_eq!(ago(0, now), "never");
        assert_eq!(ago(now - 59_000, now), "just now");
        assert_eq!(ago(now + 5_000, now), "just now");
        assert_eq!(ago(now - 5 * 60_000, now), "5 min ago");
        assert_eq!(ago(now - 47 * 3_600_000, now), "47 h ago");
        assert_eq!(ago(now - 3 * 86_400_000, now), "3 days ago");
    }

    #[test]
    fn sizes_read_in_decimal_units() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(1_000), "1.0 kB");
        assert_eq!(size(999_949), "999.9 kB");
        assert_eq!(size(999_950), "1.0 MB");
        assert_eq!(size(1_500_000_000), "1.5 GB");
        assert_eq!(size(u64::MAX), "18446.7 PB");
    }

    #[test]
    fn the_usage_report_lists_each_space_and_its_total() {
        let space = SpaceInfo {
            space_id: ID.into(),
            name: "Home\u{1b}[2J".into(),
            ..Default::default()
        };
        let usage = SpaceUsage {
            live_files: 1_204,
            live_bytes: 3_200_000_000,
            trashed_files: 1,
            trash_bytes: 40_100_000,
            old_versions: 30,
            old_version_bytes: 12_000_000,
            unreferenced_bytes: 1_500_000,
        };
        assert_eq!(
            usage_report(&[(space.clone(), usage)]),
            format!(
                "Home\\u{{1b}}[2J ({ID})\n\
                 \x20 Live files        1204 files     3.2 GB\n\
                 \x20 Trash                 1 file    40.1 MB\n\
                 \x20 Old versions     30 versions    12.0 MB\n\
                 \x20 Unreferenced                     1.5 MB\n\
                 \x20 Total                            3.3 GB\n\
                 \n\
                 Each stored chunk counts once, under the first of live files, trash and old versions that holds it.\n\
                 Unreferenced chunks are uploads not committed yet and chunks waiting out the grace period before deletion.\n"
            )
        );
        assert_eq!(usage_report(&[]), "No spaces.\n");
        let json = serde_json::to_value(UsageData::of(&space, &usage)).unwrap();
        assert_eq!(json["total_bytes"], 3_253_600_000u64);
        assert_eq!(json["trashed_files"], 1);
    }

    #[test]
    fn the_space_list_shows_each_space_by_id_and_name() {
        let spaces = [SpaceInfo {
            space_id: ID.into(),
            name: "Notes\u{1b}[2J".into(),
            ..SpaceInfo::default()
        }];
        assert_eq!(
            space_table(&spaces),
            format!("ID                                    NAME\n{ID}  Notes\\u{{1b}}[2J\n")
        );
        assert_eq!(
            space_table(&[]),
            "No spaces. Create one with koi space create <name>.\n"
        );
    }

    #[test]
    fn usage_output_shows_only_the_spaces_that_answered() {
        let answered = [(
            SpaceInfo {
                space_id: ID.into(),
                name: "Home".into(),
                ..Default::default()
            },
            SpaceUsage {
                live_files: 2,
                live_bytes: 300,
                ..Default::default()
            },
        )];
        let refused = || {
            vec![(
                SpaceInfo {
                    space_id: "s2".into(),
                    name: "Closed".into(),
                    ..Default::default()
                },
                CliError::from(ApiError::Status { status: 503 }),
            )]
        };

        let empty = usage_done(&[], &[]).unwrap();
        assert_eq!(empty.text, "No spaces.\n");
        assert_eq!(
            empty.data["spaces"],
            serde_json::json!([]),
            "an account with no spaces"
        );

        let none = usage_done(&[], &refused()).unwrap_err();
        let none = none.partial.expect("the failure keeps what was read");
        assert_eq!(none.text, "", "every space refused");
        assert_eq!(
            none.data["spaces"],
            serde_json::json!([]),
            "every space refused"
        );

        let error = usage_done(&answered, &refused()).unwrap_err();
        assert_eq!(error.code, Code::Unavailable);
        assert!(
            error.message.starts_with("no usage for Closed (s2): "),
            "{error:?}"
        );
        let partial = error.partial.expect("the failure keeps what was read");
        let spaces = partial.data["spaces"].as_array().unwrap();
        assert_eq!(spaces.len(), 1, "{:#}", partial.data);
        assert_eq!(spaces[0]["space_id"], ID);
        assert_eq!(spaces[0]["live_bytes"], 300);
        assert_eq!(partial.data["refused"][0]["space_id"], "s2");
        assert!(partial.text.starts_with(&format!("Home ({ID})\n")));
        assert_eq!(
            partial.data["spaces"],
            usage_done(&answered, &[]).unwrap().data["spaces"],
            "a refused space adds nothing to the answered ones"
        );
    }

    #[tokio::test]
    async fn a_refused_credential_stops_the_usage_and_keeps_what_was_read() {
        use crate::setup::fake::{Fake, proto};
        use koidrive_proto::v1::{AccessToken, error::Code as ProtoCode};
        let refusal = || {
            let mut reply = proto(
                "GET",
                "/v1/spaces/b/usage",
                koidrive_proto::v1::Error {
                    code: ProtoCode::Unauthenticated as i32,
                    ..Default::default()
                },
            );
            reply.status = 401;
            reply
        };
        let token = || {
            let token = AccessToken {
                token: "fixture-only".into(),
                expires_at_ms: i64::MAX,
                ..Default::default()
            };
            proto("POST", "/v1/token", token)
        };
        let read = SpaceUsage {
            live_bytes: 42,
            ..Default::default()
        };
        let fake = Fake::start(vec![
            token(),
            token(),
            proto("GET", "/v1/spaces/a/usage", read),
            refusal(),
            refusal(),
        ])
        .await;
        let auth = Auth::new(
            koidrive_net::Api::local_development(fake.base.clone()).unwrap(),
            koidrive_net::Credentials {
                user_id: "fixture-user".into(),
                device_id: "fixture-device".into(),
                refresh_credential: "fixture-only".into(),
            },
        );
        let spaces = ["a", "b", "c"]
            .into_iter()
            .map(|id| SpaceInfo {
                space_id: id.into(),
                name: id.into(),
                ..Default::default()
            })
            .collect();

        let error = usage(&auth, spaces).await.unwrap_err();

        assert_eq!(error.code, Code::AuthRequired, "{error:?}");
        let partial = error.partial.expect("the failure keeps what was read");
        let spaces = partial.data["spaces"].as_array().unwrap();
        assert_eq!(spaces.len(), 1, "{:#}", partial.data);
        assert_eq!(spaces[0]["space_id"], "a");
        assert_eq!(spaces[0]["live_bytes"], 42);
        assert_eq!(partial.data["refused"], serde_json::json!([]));
    }

    #[test]
    fn the_table_marks_this_device_and_revoked_ones() {
        let now = 1_000 * 86_400_000;
        let device = |name: &str, current, last_seen_ms, revoked_at_ms| DeviceInfo {
            device_id: ID.into(),
            name: name.into(),
            platform: "linux".into(),
            kind: Kind::Device as i32,
            last_seen_ms,
            revoked_at_ms,
            current,
            ..Default::default()
        };
        let table = device_table(
            &[
                device("desk", true, now - 120_000, 0),
                device("lost laptop", false, 0, now - 2 * 86_400_000),
                device("phone", false, now - 7_200_000, 0),
            ],
            now,
        );
        let id = ID;
        assert_eq!(
            table,
            format!(
                "ID                                    NAME         PLATFORM  LAST SEEN  STATE\n\
                 {id}  desk         linux     2 min ago  this device\n\
                 {id}  lost laptop  linux     never      revoked 2 days ago\n\
                 {id}  phone        linux     2 h ago    active\n"
            )
        );
    }
}
