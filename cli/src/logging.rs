// SPDX-License-Identifier: Apache-2.0

use crate::{diagnostics, logfile};
use clap::ValueEnum;
use std::io::IsTerminal;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum LogFormat {
    Text,
    Json,
}

pub(crate) fn init_logging(
    daemon: Option<LogFormat>,
    json: bool,
    agent_log: Option<logfile::CappedLog>,
    human: Option<&str>,
) {
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
    if let Some(command) = human.filter(|_| std::env::var_os("KOIDRIVE_LOG").is_none()) {
        builder
            .event_format(diagnostics::Human {
                command: command.to_owned(),
            })
            .init();
        return;
    }
    match daemon {
        Some(LogFormat::Json) => builder.json().with_current_span(false).init(),
        _ => builder.init(),
    }
}

/// Whether log lines carry ANSI colours: only on a terminal, and never under `--json`.
pub(crate) fn log_ansi(stderr_tty: bool, json: bool) -> bool {
    stderr_tty && !json
}
