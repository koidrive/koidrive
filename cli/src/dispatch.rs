// SPDX-License-Identifier: Apache-2.0

use crate::args::{Command, ServiceAction};
use crate::daemon::daemon;
use crate::device::device;
use crate::error::CliError;
use crate::folder::folder;
use crate::profile::paired;
use crate::report::{Done, Empty};
use crate::resolve::resolve;
use crate::space::space;
use crate::status::status;
use crate::sync_control::{pause, resume};
use crate::{
    diagnose, history, ipc, pairing, setup, share, telemetry, terminal, uninstall, update,
};
use koidrive_driver::Paths;
use koidrive_ipc::Request;
use koidrive_telemetry::schema::SetupStep;
use std::sync::Arc;

/// How a command shows in usage reports: its name, the setup flow it runs when that is known before it starts, and
/// its `--telemetry` flag. `koi setup` learns its flow from the person's choice.
pub(crate) fn telemetry_of(
    command: &Command,
) -> Option<(
    koidrive_telemetry::schema::Command,
    Option<koidrive_telemetry::schema::SetupFlow>,
    Option<telemetry::Switch>,
)> {
    use koidrive_telemetry::schema::{Command as Name, SetupFlow as Flow};
    use setup::ServerAction;
    Some(match command {
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
        Command::Update { .. } => (Name::Update, None, None),
        Command::Daemon { .. } => (Name::Daemon, None, None),
        Command::Uninstall { .. } | Command::CheckUpdates => return None,
    })
}

pub(crate) async fn execute(
    paths: Paths,
    command: Command,
    json: bool,
    verbose: bool,
    terminal: Arc<terminal::Terminal>,
) -> Result<Done, CliError> {
    match command {
        Command::Daemon { .. } => daemon(paths, json).await,
        Command::Status { check } => status(&paths, check).await,
        Command::Diagnose => diagnose::run(&paths).await,
        Command::Update { verbose } => update::run(&paths, json, verbose).await,
        Command::Uninstall {
            yes,
            keep_signed_in,
            last_device,
        } => {
            let options = uninstall::Options {
                yes,
                keep_signed_in,
                last_device,
            };
            uninstall::run(&paths, options, &terminal).await
        }
        Command::Resolve { id, answer } => resolve(&paths, id, answer, json).await,
        Command::Setup {
            device,
            login,
            script,
            ..
        } => {
            setup::setup(
                &paths,
                &device.name(),
                &login,
                &script,
                json,
                verbose,
                terminal,
            )
            .await
        }
        Command::Join {
            code,
            code_stdin,
            server,
            device,
        } => {
            let join = async {
                let code = setup::read_code(code, code_stdin).await?;
                let invitation =
                    pairing::parse(&code, server.as_deref()).map_err(|message| {
                        CliError::usage(message).human("The pairing code is incomplete or its server address is invalid. Copy the full code, including @server, or provide the server with --server.")
                    })?;
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
        Command::Server { action } => setup::server(&paths, action, json, verbose, terminal).await,
        Command::Space { action } => space(paths, action).await,
        Command::Folder { action } => folder(&paths, action).await,
        Command::Service {
            action: ServiceAction::Start,
        }
        | Command::Reconcile { .. } => Ok(Done::ok(Empty {}, "")),
        Command::Pause => pause(&paths).await,
        Command::Resume => resume(&paths).await,
        Command::CheckUpdates => update::check_updates(&paths).await,
        Command::Device { action } => device(paths, action).await,
        Command::History { action } => history::run(&paths, action).await,
        Command::Share { action } => share::run(&paths, action).await,
        Command::Telemetry { action } => telemetry::command(&paths, action).await,
    }
}
