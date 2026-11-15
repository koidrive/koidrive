// SPDX-License-Identifier: Apache-2.0

//! `koi setup` and `koi server ...`: log in with Cloudflare, then deploy a server to the user's account, add
//! this device to the server there, upgrade it or delete it (docs/design/cli-setup.md). The Cloudflare grant lives in
//! memory for one command and is revoked when the command ends, also on failure, Ctrl-C and SIGTERM (section 7).
//!
//! Nothing here asks a question unless [`mode`] says the command is guided: stdin and stderr are terminals, and
//! neither `--json` nor the `CI` variable rules it out, or `--interactive` asks for it.

mod bundle;
#[cfg(test)]
mod bundle_files;
mod claim;
mod deploy;
#[cfg(test)]
pub(crate) mod fake;
mod onboarding;
mod remote;
mod remove;
mod servers;
mod session;
mod state;
#[cfg(feature = "test-setup")]
mod testing;
#[cfg(test)]
mod tests {
    include!("setup/tests.rs");
}
mod uninstall;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use koidrive_cloudflare::{Account, Cloudflare, Grant, Jurisdiction, Script};
use koidrive_proto::v1::ServerInfo;
use url::Url;

use crate::DeviceArgs;
use crate::error::CliError;
use crate::terminal::Terminal;

pub(crate) use bundle::embedded_bundle;
use onboarding::report_paired;
pub use onboarding::setup;
pub(crate) use onboarding::shown_folder;
use remote::server_api;
use servers::jurisdiction;
pub use servers::server;
pub(crate) use session::read_code;

/// Marks the Worker scripts `koi setup` uploads (section 6).
const TAG: &str = "koidrive-server";

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
