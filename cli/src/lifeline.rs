// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use koidrive_driver::Paths;
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use rustix::process::{Signal, getpid, kill_process};

use crate::service;

pub(crate) const LIFELINE: &str = "KOIDRIVE_TEST_LIFELINE";

enum Refusal {
    Ended,
    AccountHome(PathBuf, PathBuf),
}

pub(crate) fn hold() -> Option<ExitCode> {
    let dir = PathBuf::from(std::env::var_os(LIFELINE)?);
    match tie(&dir) {
        Ok((alive, held)) => {
            std::thread::spawn(move || {
                let _alive = alive;
                while flock(&held, FlockOperation::LockShared) == Err(Errno::INTR) {}
                let _ = kill_process(getpid(), Signal::KILL);
            });
            None
        }
        Err(Refusal::Ended) => Some(ExitCode::SUCCESS),
        Err(Refusal::AccountHome(path, account)) => {
            eprintln!(
                "koi: a test must not use {}, which is in this account's home {}",
                path.display(),
                account.display()
            );
            Some(ExitCode::from(78))
        }
    }
}

fn tie(dir: &Path) -> Result<(File, File), Refusal> {
    let open = |name: &str| File::open(dir.join(name)).map_err(|_| Refusal::Ended);
    let alive = open("alive")?;
    flock(&alive, FlockOperation::NonBlockingLockShared).map_err(|_| Refusal::Ended)?;
    let held = open("held")?;
    if flock(&held, FlockOperation::NonBlockingLockShared).is_ok() {
        return Err(Refusal::Ended);
    }
    if let Some(account) = service::account_home() {
        let profile = Paths::from_env().ok();
        let used = (std::env::var_os("HOME").map(PathBuf::from).into_iter()).chain(
            profile
                .into_iter()
                .flat_map(|paths| [paths.state_dir, paths.config_file]),
        );
        if let Some(path) = used.into_iter().find(|path| path.starts_with(&account)) {
            return Err(Refusal::AccountHome(path, account));
        }
    }
    Ok((alive, held))
}
