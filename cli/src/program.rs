// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::path::Path;

pub(crate) fn system(name: &'static str) -> std::process::Command {
    std::process::Command::new(name)
}

pub(crate) fn system_async(name: &'static str) -> tokio::process::Command {
    tokio::process::Command::new(name)
}

#[cfg(target_os = "macos")]
pub(crate) fn configured_async(tool: &Path) -> tokio::process::Command {
    tokio::process::Command::new(tool)
}

pub(crate) fn found(program: &Path) -> io::Result<std::process::Command> {
    crate::lifeline::confine_program(program)?;
    Ok(std::process::Command::new(program))
}

pub(crate) fn found_async(program: &Path) -> io::Result<tokio::process::Command> {
    crate::lifeline::confine_program(program)?;
    Ok(tokio::process::Command::new(program))
}
