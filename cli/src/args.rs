// SPDX-License-Identifier: Apache-2.0

use crate::device::DeviceAction;
use crate::folder::FolderAction;
use crate::logging::LogFormat;
use crate::options::DeviceArgs;
use crate::space::SpaceAction;
use crate::{history, setup, share, telemetry, terminal};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "koi",
    version,
    color = terminal::clap_color(),
    about = "Koidrive sync client.",
    long_about = "Koidrive keeps your folders in sync through your own server.",
    after_help = "Commands ask questions only when a terminal is attached to stdin and stderr.\nAgents: start with koi status --json."
)]
pub(crate) struct Cli {
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
    pub(crate) json: Option<u32>,
    #[arg(
        long,
        global = true,
        help = "Use line-oriented output without color or animation"
    )]
    pub(crate) plain: bool,
    /// Show technical details in terminal results.
    #[arg(long, global = true)]
    pub(crate) verbose: bool,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand)]
pub(crate) enum Command {
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
    Update {
        /// Print each step's full output, such as the installer's download checks and Homebrew's log.
        #[arg(long)]
        verbose: bool,
    },
    /// Remove Koidrive from this computer: sign this device out, stop the background service, and delete the app, the
    /// koi command, the settings, the sync state and the logs. The files in your folders stay.
    Uninstall {
        /// Remove without asking.
        #[arg(long)]
        yes: bool,
        /// Keep this device on the server's device list instead of signing it out.
        #[arg(long)]
        keep_signed_in: bool,
        /// Sign out even if this is the account's last active device or the server does not answer.
        #[arg(long, conflicts_with = "keep_signed_in")]
        last_device: bool,
    },
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
    /// Ask GitHub now whether a newer release is out, through background sync when it runs.
    #[command(name = "_check-updates", hide = true)]
    CheckUpdates,
}

impl Command {
    /// Whether the command reports the decisions pending in the profile after its own output: on stderr as a banner,
    /// or in the JSON envelope. `status` and a `resolve` that lists show the decisions themselves, and the daemon never
    /// ends. A `resolve` that answers reports the decisions still pending.
    pub(crate) fn reports_pending(&self) -> bool {
        !matches!(
            self,
            Command::Daemon { .. }
                | Command::Status { .. }
                | Command::Resolve { id: None, .. }
                | Command::Uninstall { .. }
        )
    }
}

impl Cli {
    /// Whether the command ends with the pending-decision banner on stderr. JSON output carries decisions as data,
    /// without decoration, and `resolve` names itself.
    pub(crate) fn shows_banner(&self) -> bool {
        self.json.is_none()
            && (self.command.as_ref()).is_some_and(|command| {
                command.reports_pending() && !matches!(command, Command::Resolve { .. })
            })
    }
}

#[derive(Subcommand)]
pub(crate) enum ServiceAction {
    /// Install and start the service, after fixing what kept it from running, such as koi switched off in Login Items.
    Start,
}

pub(crate) fn schema_version(text: &str) -> Result<u32, String> {
    match text {
        "1" => Ok(1),
        _ => Err("only JSON schema 1 exists".to_owned()),
    }
}
