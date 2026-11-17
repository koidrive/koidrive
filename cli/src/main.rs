// SPDX-License-Identifier: Apache-2.0

//! The `koi` command: shows status and pending decisions, sets up a server or joins one, links folders, lists a
//! folder's trash and a file's versions, manages the account's devices and spaces, and runs the sync daemon. Commands
//! run in the daemon through its socket when it is running, and on their own otherwise. The device, space, trash and
//! versions commands talk to the server directly with the credential. With `--json` every command prints one
//! envelope (see [`report`]) and exits with the status of its [`koidrive_ipc::Code`].

mod app;
mod app_install;
mod args;
mod completion;
mod daemon;
mod device;
mod diagnose;
mod diagnostics;
mod dispatch;
mod error;
mod folder;
mod formatting;
mod gitignore;
mod history;
mod ipc;
mod lifecycle;
mod lifeline;
mod logfile;
mod logging;
mod manifest;
mod notify;
mod options;
mod output;
mod pairing;
mod profile;
mod program;
mod report;
mod request;
mod resolve;
#[cfg(test)]
mod schema;
mod service;
mod setup;
mod share;
mod space;
mod status;
mod sync_control;
mod telemetry;
mod terminal;
mod uninstall;
mod update;

use crate::args::{Cli, Command, schema_version};
use crate::completion::finish;
use crate::device::devices;
use crate::dispatch::telemetry_of;
use crate::error::CliError;
use crate::folder::{absolute, link, linked, linked_summary};
use crate::formatting::{ago, count, need_you, now_ms, printable, size};
use crate::lifecycle::run;
use crate::logging::{LogFormat, init_logging};
use crate::options::short_name;
use crate::profile::{reading, signed_in, unexpected};
use crate::report::{Done, HelpData, VersionInfoData};
#[cfg(test)]
use crate::resolve::resolve;
use crate::resolve::{RESOLVE, choices, pending_questions, question_notice};
use crate::space::{Named, named_space, total_bytes};
use crate::status::status;
#[cfg(test)]
use crate::sync_control::resume;
use clap::error::ErrorKind;
use clap::{ArgAction, CommandFactory, Parser};
use koidrive_driver::{DriverError, Paths};
use koidrive_ipc::Code;
use std::ffi::OsString;
use std::io;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::signal::unix::{SignalKind, signal};

pub use options::{DeviceArgs, RequestArg};

fn main() -> ExitCode {
    if let Some(exit) = lifeline::hold() {
        return exit;
    }
    let args: Vec<OsString> = std::env::args_os().collect();
    if let Some(exit) = terminal::helper(&args) {
        return exit;
    }
    let command = command_path(&args);
    if let Some(error) = json_versions(&args).find_map(Result::err) {
        return finish(
            &command,
            true,
            false,
            Err(CliError::usage(error)),
            Vec::new(),
        );
    }
    if let Some(names) = help_names(&args).filter(|_| json_requested(&args)) {
        let path = names.join(" ");
        let outcome = help_of(&path)
            .map(|help| help_done(help, &path))
            .ok_or_else(|| CliError::usage(format!("no command is named {}", printable(&path))));
        return finish(
            format!("help {path}").trim_end(),
            true,
            false,
            outcome,
            Vec::new(),
        );
    }
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => return parse_failure(error, &args, &command, json_requested(&args)),
    };
    let json = cli.json.is_some();
    let verbose = cli.verbose;
    let shows_banner = cli.shows_banner();
    let Some(command_args) = cli.command else {
        let help = Cli::command().render_help().to_string();
        return finish(&command, json, verbose, Ok(help_done(help, "")), Vec::new());
    };
    let reports_pending = command_args.reports_pending();
    let interactive = match &command_args {
        Command::Setup { login, .. } => login.login.interactive,
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
        Err(error) => {
            return finish(
                &command,
                json,
                verbose,
                Err(CliError::from(DriverError::from(error))),
                Vec::new(),
            );
        }
    };
    let agent_log = (log_format.is_some())
        .then(|| service::this_service(&paths).ok())
        .flatten()
        .and_then(|service| logfile::CappedLog::of_stderr(service.log_file()));
    init_logging(
        log_format,
        json || !console::colors_enabled_stderr(),
        agent_log,
        (!json && !verbose && output::concise(&command)).then_some(command.as_str()),
    );
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let error = CliError::from(format!("cannot start: {error}"));
            return finish(&command, json, verbose, Err(error), Vec::new());
        }
    };
    let daemon_run = matches!(command_args, Command::Daemon { .. });
    let pending_paths = paths.clone();
    let telemetry_paths = paths.clone();
    let telemetry = telemetry_of(&command_args);
    telemetry::vote_with_each_token_refresh(&paths);
    let sets_up = matches!(
        command_args,
        Command::Setup { .. }
            | Command::Join { .. }
            | Command::Server {
                action: setup::ServerAction::Deploy { .. }
                    | setup::ServerAction::AddDevice { .. }
                    | setup::ServerAction::Claim { .. }
            }
    );
    let ask = guided
        && !daemon_run
        && !matches!(command_args, Command::Uninstall { .. })
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
                if let Some((name, flow, switch)) = telemetry {
                    telemetry::start(&paths, switch, name, flow, ask).await;
                }
                let mut outcome = run(paths, command_args, json, verbose, work_terminal.clone()).await;
                if let Err(error) = &outcome {
                    telemetry::command_failed(error.code);
                }
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
                let exit = finish(command_name, json, verbose, outcome, pending);
                if !(ask && (sets_up || prompt_eligible)) {
                    telemetry::forget();
                } else if let Err(error) = telemetry::ask(&telemetry_paths, &work_terminal).await {
                    return finish(command_name, json, verbose, Err(error), Vec::new());
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
                ).human(format!("{}. Changes already made were kept. Run koi status to check this device before continuing.", if status == 143 { "Stopped" } else { "Interrupted" }));
                telemetry::forget();
                telemetry::flush();
                let _ = finish(&command, json, verbose, Err(error), Vec::new());
                let _ = io::Write::flush(&mut io::stdout());
                std::process::exit(status.into());
            }
        }
    })
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
        output::print(
            command,
            output::Presentation::Text,
            output::Tone::Attention,
            "",
            output::Note::Usage(&rendered),
            None,
        );
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
    finish(command, json, false, outcome, Vec::new())
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
    Done {
        presentation: output::Presentation::Literal,
        ..Done::ok(data, help)
    }
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

#[cfg(test)]
mod tests;
