// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

pub(super) fn write(path: &Path, text: &str) -> Result<bool, String> {
    if fs::read(path).is_ok_and(|old| old == text.as_bytes()) {
        return Ok(false);
    }
    let failed = |error: io::Error| format!("cannot write {}: {error}", path.display());
    let dir = path.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(dir).map_err(failed)?;
    koidrive_state_file::write(path, text.as_bytes()).map_err(failed)?;
    Ok(true)
}

pub(super) fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(format!("cannot remove {}: {error}", path.display()))
        }
        _ => Ok(()),
    }
}

pub(super) fn run(program: &str, args: &[&str]) -> Result<Output, String> {
    Command::new(program)
        .args(args)
        .output()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => format!("{program} is not installed"),
            _ => format!("cannot run {program}: {error}"),
        })
}

pub(super) fn ok(program: &str, args: &[&str]) -> Result<String, String> {
    let output = run(program, args)?;
    match output.status.success() {
        true => Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned()),
        false => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let cause = match stderr.trim() {
                "" => output.status.to_string(),
                text => text.to_owned(),
            };
            Err(format!("{program} {} failed: {cause}", args.join(" ")))
        }
    }
}
